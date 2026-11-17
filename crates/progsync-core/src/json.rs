//! JSON: values, a parser, a writer, and conversions for psync's types.
//!
//! - [`Value`] is a parsed document; objects are [`Map`]s with sorted keys.
//! - [`ToJson`] writes a type as compact JSON. Structs (see [`json_struct!`])
//!   write their fields in declaration order, so the bytes of stored
//!   objects (revisions, operations, tasks) are deterministic.
//! - [`FromJson`] reads a type from a [`Value`]; unknown object fields are
//!   ignored and fields marked `#default` may be missing, so formats can grow.
//! - [`json!`](crate::json!) builds a [`Value`] from JSON-like syntax.
//!
//! Output format: no spaces in compact form; pretty form indents by two
//! spaces with `"key": value`; strings escape `"`, `\`, and control
//! characters (`\n`, `\t`, ... or `\u00XX`) and keep other characters as is.

use crate::error::{Error, Result};
use crate::err;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

/// A JSON object (keys sorted).
pub type Map = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Value>),
    Object(Map),
}

/// A JSON number. Integers stay exact: non-negative ones as `UInt`,
/// negative ones as `Int`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    UInt(u64),
    Int(i64),
    Float(f64),
}

impl Number {
    pub fn is_u64(&self) -> bool {
        matches!(self, Number::UInt(_))
    }

    pub fn is_i64(&self) -> bool {
        match self {
            Number::UInt(n) => *n <= i64::MAX as u64,
            Number::Int(_) => true,
            Number::Float(_) => false,
        }
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        Value::Number(*self).write_json(&mut s);
        f.write_str(&s)
    }
}

static NULL: Value = Value::Null;

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.get(key),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Object(m) => m.get_mut(key),
            _ => None,
        }
    }

    /// The value at a JSON pointer such as `/clientInfo/name` or `/files/0`.
    pub fn pointer(&self, ptr: &str) -> Option<&Value> {
        if ptr.is_empty() {
            return Some(self);
        }
        let mut cur = self;
        for part in ptr.strip_prefix('/')?.split('/') {
            let part = part.replace("~1", "/").replace("~0", "~");
            cur = match cur {
                Value::Object(m) => m.get(&part)?,
                Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(Number::UInt(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(Number::UInt(n)) => i64::try_from(*n).ok(),
            Value::Number(Number::Int(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(Number::UInt(n)) => Some(*n as f64),
            Value::Number(Number::Int(n)) => Some(*n as f64),
            Value::Number(Number::Float(f)) => Some(*f),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }

    /// Take the value, leaving `null`.
    pub fn take(&mut self) -> Value {
        std::mem::take(self)
    }
}

impl std::ops::Index<&str> for Value {
    type Output = Value;
    /// `null` when absent or not an object.
    fn index(&self, key: &str) -> &Value {
        self.get(key).unwrap_or(&NULL)
    }
}

impl std::ops::Index<usize> for Value {
    type Output = Value;
    fn index(&self, i: usize) -> &Value {
        self.as_array().and_then(|a| a.get(i)).unwrap_or(&NULL)
    }
}

impl std::ops::IndexMut<&str> for Value {
    /// Inserts `null` when absent; a `null` becomes an empty object first.
    fn index_mut(&mut self, key: &str) -> &mut Value {
        if self.is_null() {
            *self = Value::Object(Map::new());
        }
        match self {
            Value::Object(m) => m.entry(key.to_string()).or_insert(Value::Null),
            other => panic!("cannot index {} with a key", kind(other)),
        }
    }
}

impl std::ops::IndexMut<usize> for Value {
    fn index_mut(&mut self, i: usize) -> &mut Value {
        match self {
            Value::Array(a) => &mut a[i],
            other => panic!("cannot index {} with a number", kind(other)),
        }
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

macro_rules! value_eq {
    ($($t:ty => |$v:ident, $o:ident| $e:expr),* $(,)?) => {$(
        impl PartialEq<$t> for Value {
            fn eq(&self, other: &$t) -> bool {
                let ($v, $o) = (self, other);
                $e
            }
        }
        impl PartialEq<Value> for $t {
            fn eq(&self, other: &Value) -> bool {
                other == self
            }
        }
    )*};
}

value_eq! {
    str => |v, o| v.as_str() == Some(o),
    &str => |v, o| v.as_str() == Some(*o),
    String => |v, o| v.as_str() == Some(o.as_str()),
    bool => |v, o| v.as_bool() == Some(*o),
    i32 => |v, o| v.as_i64() == Some(i64::from(*o)),
    i64 => |v, o| v.as_i64() == Some(*o),
    u32 => |v, o| v.as_u64() == Some(u64::from(*o)),
    u64 => |v, o| v.as_u64() == Some(*o),
    usize => |v, o| v.as_u64() == Some(*o as u64),
}

macro_rules! value_from {
    ($($t:ty => |$x:ident| $e:expr),* $(,)?) => {$(
        impl From<$t> for Value {
            fn from($x: $t) -> Value {
                $e
            }
        }
    )*};
}

value_from! {
    &str => |x| Value::String(x.to_string()),
    String => |x| Value::String(x),
    &String => |x| Value::String(x.clone()),
    bool => |x| Value::Bool(x),
    u8 => |x| Value::Number(Number::UInt(x.into())),
    u32 => |x| Value::Number(Number::UInt(x.into())),
    u64 => |x| Value::Number(Number::UInt(x)),
    usize => |x| Value::Number(Number::UInt(x as u64)),
    i32 => |x| Value::from(i64::from(x)),
    i64 => |x| Value::Number(if x >= 0 { Number::UInt(x as u64) } else { Number::Int(x) }),
    f64 => |x| Value::Number(Number::Float(x)),
    Map => |x| Value::Object(x),
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Value {
        Value::Array(v.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map_or(Value::Null, Into::into)
    }
}

impl fmt::Display for Value {
    /// Compact JSON; `{:#}` is pretty-printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write_json(&mut s);
        if f.alternate() {
            f.write_str(&pretty(&s))
        } else {
            f.write_str(&s)
        }
    }
}

// ---- writing -----------------------------------------------------------------

/// A type that can be written as JSON.
pub trait ToJson {
    /// Append compact JSON to `out`.
    fn write_json(&self, out: &mut String);

    /// As a [`Value`] (objects then have sorted keys).
    fn to_json(&self) -> Value {
        let mut s = String::new();
        self.write_json(&mut s);
        parse(&s).expect("written JSON parses")
    }
}

/// Append `s` as a JSON string.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        let esc = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0..=0x1f => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if esc.is_empty() {
            out.push_str(&format!("\\u{b:04x}"));
        } else {
            out.push_str(esc);
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

fn write_f64(out: &mut String, f: f64) {
    if !f.is_finite() {
        out.push_str("null");
        return;
    }
    let s = if f != 0.0 && (f.abs() >= 1e16 || f.abs() < 1e-5) { format!("{f:e}") } else { format!("{f}") };
    out.push_str(&s);
    if !s.contains(['.', 'e', 'E']) {
        out.push_str(".0");
    }
}

impl ToJson for Value {
    fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => b.write_json(out),
            Value::Number(Number::UInt(n)) => n.write_json(out),
            Value::Number(Number::Int(n)) => n.write_json(out),
            Value::Number(Number::Float(f)) => write_f64(out, *f),
            Value::String(s) => write_str(out, s),
            Value::Array(a) => a.write_json(out),
            Value::Object(m) => m.write_json(out),
        }
    }

    fn to_json(&self) -> Value {
        self.clone()
    }
}

impl ToJson for bool {
    fn write_json(&self, out: &mut String) {
        out.push_str(if *self { "true" } else { "false" });
    }
    fn to_json(&self) -> Value {
        Value::Bool(*self)
    }
}

macro_rules! int_to_json {
    ($($t:ty),*) => {$(
        impl ToJson for $t {
            fn write_json(&self, out: &mut String) {
                out.push_str(&self.to_string());
            }
            fn to_json(&self) -> Value {
                let n = *self as i128;
                Value::Number(if n >= 0 { Number::UInt(n as u64) } else { Number::Int(n as i64) })
            }
        }
    )*};
}

int_to_json!(u8, u16, u32, u64, usize, i8, i16, i32, i64);

impl ToJson for f64 {
    fn write_json(&self, out: &mut String) {
        write_f64(out, *self);
    }
}

impl ToJson for str {
    fn write_json(&self, out: &mut String) {
        write_str(out, self);
    }
    fn to_json(&self) -> Value {
        Value::String(self.to_string())
    }
}

impl ToJson for String {
    fn write_json(&self, out: &mut String) {
        write_str(out, self);
    }
    fn to_json(&self) -> Value {
        Value::String(self.clone())
    }
}

impl ToJson for Path {
    fn write_json(&self, out: &mut String) {
        write_str(out, &self.to_string_lossy());
    }
}

impl ToJson for PathBuf {
    fn write_json(&self, out: &mut String) {
        self.as_path().write_json(out);
    }
}

impl<T: ToJson + ?Sized> ToJson for &T {
    fn write_json(&self, out: &mut String) {
        (**self).write_json(out);
    }
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}

impl<T: ToJson + ?Sized> ToJson for Box<T> {
    fn write_json(&self, out: &mut String) {
        (**self).write_json(out);
    }
}

impl<T: ToJson> ToJson for Option<T> {
    fn write_json(&self, out: &mut String) {
        match self {
            Some(x) => x.write_json(out),
            None => out.push_str("null"),
        }
    }
    fn to_json(&self) -> Value {
        self.as_ref().map_or(Value::Null, ToJson::to_json)
    }
}

impl<T: ToJson> ToJson for [T] {
    fn write_json(&self, out: &mut String) {
        out.push('[');
        for (i, x) in self.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            x.write_json(out);
        }
        out.push(']');
    }
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(ToJson::to_json).collect())
    }
}

impl<T: ToJson, const N: usize> ToJson for [T; N] {
    fn write_json(&self, out: &mut String) {
        self.as_slice().write_json(out);
    }
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}

impl<T: ToJson> ToJson for Vec<T> {
    fn write_json(&self, out: &mut String) {
        self.as_slice().write_json(out);
    }
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}

impl<T: ToJson> ToJson for std::collections::BTreeSet<T> {
    fn write_json(&self, out: &mut String) {
        out.push('[');
        for (i, x) in self.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            x.write_json(out);
        }
        out.push(']');
    }
}

impl<A: ToJson, B: ToJson> ToJson for (A, B) {
    fn write_json(&self, out: &mut String) {
        out.push('[');
        self.0.write_json(out);
        out.push(',');
        self.1.write_json(out);
        out.push(']');
    }
}

fn write_entries<'a, K: AsRef<str> + 'a, V: ToJson + 'a>(out: &mut String, it: impl Iterator<Item = (&'a K, &'a V)>) {
    out.push('{');
    for (i, (k, v)) in it.enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(out, k.as_ref());
        out.push(':');
        v.write_json(out);
    }
    out.push('}');
}

impl<K: AsRef<str> + Ord, V: ToJson> ToJson for BTreeMap<K, V> {
    fn write_json(&self, out: &mut String) {
        write_entries(out, self.iter());
    }
    fn to_json(&self) -> Value {
        Value::Object(self.iter().map(|(k, v)| (k.as_ref().to_string(), v.to_json())).collect())
    }
}

impl<K: AsRef<str> + Ord + std::hash::Hash, V: ToJson> ToJson for HashMap<K, V> {
    /// Keys sorted, for deterministic output.
    fn write_json(&self, out: &mut String) {
        let mut e: Vec<_> = self.iter().collect();
        e.sort_by(|a, b| a.0.cmp(b.0));
        write_entries(out, e.into_iter());
    }
}

fn compact<T: ToJson + ?Sized>(x: &T) -> String {
    let mut s = String::new();
    x.write_json(&mut s);
    s
}

/// Compact JSON of `x` as bytes. (Writing cannot fail; the `Result` keeps
/// call sites uniform with reading.)
pub fn to_vec<T: ToJson + ?Sized>(x: &T) -> Result<Vec<u8>> {
    Ok(compact(x).into_bytes())
}

/// Compact JSON of `x`.
pub fn to_string<T: ToJson + ?Sized>(x: &T) -> Result<String> {
    Ok(compact(x))
}

/// Pretty JSON of `x` as bytes (two-space indent).
pub fn to_vec_pretty<T: ToJson + ?Sized>(x: &T) -> Result<Vec<u8>> {
    Ok(pretty(&compact(x)).into_bytes())
}

pub fn to_value<T: ToJson>(x: T) -> Result<Value> {
    Ok(x.to_json())
}

/// Re-indent compact JSON (as produced by [`ToJson`]).
fn pretty(compact: &str) -> String {
    let b = compact.as_bytes();
    let mut out = String::with_capacity(compact.len() * 2);
    let mut depth = 0usize;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                i += 1;
                while b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                out.push_str(&compact[start..=i]);
            }
            open @ (b'{' | b'[') => {
                let close = if open == b'{' { b'}' } else { b']' };
                if b.get(i + 1) == Some(&close) {
                    out.push(open as char);
                    out.push(close as char);
                    i += 1;
                } else {
                    out.push(open as char);
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            close @ (b'}' | b']') => {
                depth -= 1;
                newline(&mut out, depth);
                out.push(close as char);
            }
            b',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            b':' => out.push_str(": "),
            _ => {
                let start = i;
                while i + 1 < b.len() && !matches!(b[i + 1], b',' | b'}' | b']' | b':') {
                    i += 1;
                }
                out.push_str(&compact[start..=i]);
            }
        }
        i += 1;
    }
    out
}

// ---- reading -----------------------------------------------------------------

/// A type that can be read from JSON.
pub trait FromJson: Sized {
    fn from_json(v: &Value) -> Result<Self>;

    /// The value when an object field is absent: none (an error) by
    /// default, `None` for options.
    fn missing() -> Option<Self> {
        None
    }
}

fn expected(what: &str, v: &Value) -> Error {
    err!("expected {what}, found {}", kind(v))
}

impl FromJson for Value {
    fn from_json(v: &Value) -> Result<Self> {
        Ok(v.clone())
    }
}

impl FromJson for bool {
    fn from_json(v: &Value) -> Result<Self> {
        v.as_bool().ok_or_else(|| expected("a boolean", v))
    }
}

macro_rules! int_from_json {
    ($($t:ty),*) => {$(
        impl FromJson for $t {
            fn from_json(v: &Value) -> Result<Self> {
                let n: i128 = match v {
                    Value::Number(Number::UInt(n)) => *n as i128,
                    Value::Number(Number::Int(n)) => *n as i128,
                    _ => return Err(expected("an integer", v)),
                };
                <$t>::try_from(n).map_err(|_| err!("integer {n} out of range for {}", stringify!($t)))
            }
        }
    )*};
}

int_from_json!(u8, u16, u32, u64, usize, i8, i16, i32, i64);

impl FromJson for f64 {
    fn from_json(v: &Value) -> Result<Self> {
        v.as_f64().ok_or_else(|| expected("a number", v))
    }
}

impl FromJson for String {
    fn from_json(v: &Value) -> Result<Self> {
        v.as_str().map(String::from).ok_or_else(|| expected("a string", v))
    }
}

impl FromJson for PathBuf {
    fn from_json(v: &Value) -> Result<Self> {
        String::from_json(v).map(PathBuf::from)
    }
}

impl<T: FromJson> FromJson for Option<T> {
    fn from_json(v: &Value) -> Result<Self> {
        if v.is_null() {
            Ok(None)
        } else {
            T::from_json(v).map(Some)
        }
    }

    fn missing() -> Option<Self> {
        Some(None)
    }
}

impl<T: FromJson> FromJson for Box<T> {
    fn from_json(v: &Value) -> Result<Self> {
        T::from_json(v).map(Box::new)
    }
}

impl<T: FromJson> FromJson for Vec<T> {
    fn from_json(v: &Value) -> Result<Self> {
        let a = v.as_array().ok_or_else(|| expected("an array", v))?;
        a.iter().enumerate().map(|(i, x)| T::from_json(x).map_err(|e| e.context(format!("item {i}")))).collect()
    }
}

impl<T: FromJson + Ord> FromJson for std::collections::BTreeSet<T> {
    fn from_json(v: &Value) -> Result<Self> {
        Ok(Vec::<T>::from_json(v)?.into_iter().collect())
    }
}

impl<A: FromJson, B: FromJson> FromJson for (A, B) {
    fn from_json(v: &Value) -> Result<Self> {
        match v.as_array().map(Vec::as_slice) {
            Some([a, b]) => Ok((A::from_json(a)?, B::from_json(b)?)),
            _ => Err(expected("an array of 2", v)),
        }
    }
}

impl<V: FromJson> FromJson for BTreeMap<String, V> {
    fn from_json(v: &Value) -> Result<Self> {
        let m = v.as_object().ok_or_else(|| expected("an object", v))?;
        m.iter().map(|(k, x)| Ok((k.clone(), V::from_json(x).map_err(|e| e.context(format!("key `{k}`")))?))).collect()
    }
}

impl<V: FromJson> FromJson for HashMap<String, V> {
    fn from_json(v: &Value) -> Result<Self> {
        Ok(BTreeMap::<String, V>::from_json(v)?.into_iter().collect())
    }
}

pub fn from_value<T: FromJson>(v: Value) -> Result<T> {
    T::from_json(&v)
}

pub fn from_str<T: FromJson>(s: &str) -> Result<T> {
    T::from_json(&parse(s)?)
}

pub fn from_slice<T: FromJson>(b: &[u8]) -> Result<T> {
    let s = std::str::from_utf8(b).map_err(|e| err!("invalid UTF-8 in JSON: {e}"))?;
    from_str(s)
}

/// Parse a JSON document.
pub fn parse(s: &str) -> Result<Value> {
    let mut p = Parser { s: s.as_bytes(), text: s, i: 0, depth: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.error("trailing characters"));
    }
    Ok(v)
}

/// Nesting deeper than this is rejected (protects the stack from hostile
/// input, e.g. on the HTTP API).
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, msg: &str) -> Error {
        let before = &self.s[..self.i.min(self.s.len())];
        let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
        let col = before.iter().rev().take_while(|&&b| b != b'\n').count() + 1;
        err!("{msg} at line {line} column {col}")
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> Result<()> {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(())
        } else {
            Err(self.error("expected value"))
        }
    }

    fn value(&mut self) -> Result<Value> {
        let Some(&c) = self.s.get(self.i) else { return Err(self.error("EOF while parsing a value")) };
        match c {
            b'n' => self.eat("null").map(|_| Value::Null),
            b't' => self.eat("true").map(|_| Value::Bool(true)),
            b'f' => self.eat("false").map(|_| Value::Bool(false)),
            b'"' => self.string().map(Value::String),
            b'[' | b'{' => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err(self.error("recursion limit exceeded"));
                }
                let v = if c == b'[' { self.array() } else { self.object() };
                self.depth -= 1;
                v
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.error("expected value")),
        }
    }

    fn array(&mut self) -> Result<Value> {
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.s.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Value::Array(out));
        }
        loop {
            self.ws();
            out.push(self.value()?);
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(out));
                }
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    fn object(&mut self) -> Result<Value> {
        self.i += 1;
        let mut out = Map::new();
        self.ws();
        if self.s.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Value::Object(out));
        }
        loop {
            self.ws();
            if self.s.get(self.i) != Some(&b'"') {
                return Err(self.error("key must be a string"));
            }
            let k = self.string()?;
            self.ws();
            if self.s.get(self.i) != Some(&b':') {
                return Err(self.error("expected `:`"));
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            out.insert(k, v);
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(out));
                }
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        let h = self.text.get(self.i..self.i + 4).ok_or_else(|| self.error("EOF while parsing a string"))?;
        let n = u32::from_str_radix(h, 16).map_err(|_| self.error("invalid escape"))?;
        self.i += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<String> {
        self.i += 1;
        let mut out = String::new();
        let mut start = self.i;
        loop {
            let Some(&c) = self.s.get(self.i) else { return Err(self.error("EOF while parsing a string")) };
            match c {
                b'"' => {
                    out.push_str(&self.text[start..self.i]);
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    out.push_str(&self.text[start..self.i]);
                    self.i += 1;
                    let Some(&e) = self.s.get(self.i) else { return Err(self.error("EOF while parsing a string")) };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut n = self.hex4()?;
                            if (0xd800..0xdc00).contains(&n) && self.s[self.i..].starts_with(b"\\u") {
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err(self.error("invalid surrogate pair"));
                                }
                                n = 0x10000 + ((n - 0xd800) << 10) + (lo - 0xdc00);
                            }
                            out.push(char::from_u32(n).ok_or_else(|| self.error("invalid unicode escape"))?);
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                    start = self.i;
                }
                0..=0x1f => return Err(self.error("control character in string")),
                _ => self.i += 1,
            }
        }
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.i;
        let digits = |p: &mut Self| {
            let s = p.i;
            while p.i < p.s.len() && p.s[p.i].is_ascii_digit() {
                p.i += 1;
            }
            p.i > s
        };
        if self.s[self.i] == b'-' {
            self.i += 1;
        }
        if self.s.get(self.i) == Some(&b'0') {
            self.i += 1;
        } else if !digits(self) {
            return Err(self.error("invalid number"));
        }
        let mut float = false;
        if self.s.get(self.i) == Some(&b'.') {
            self.i += 1;
            float = true;
            if !digits(self) {
                return Err(self.error("invalid number"));
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            float = true;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return Err(self.error("invalid number"));
            }
        }
        let t = &self.text[start..self.i];
        if !float {
            if let Ok(n) = t.parse::<u64>() {
                return Ok(Value::Number(Number::UInt(n)));
            }
            if let Ok(n) = t.parse::<i64>() {
                return Ok(Value::from(n));
            }
        }
        t.parse::<f64>().map(|f| Value::Number(Number::Float(f))).map_err(|_| self.error("invalid number"))
    }
}

// ---- macros --------------------------------------------------------------------

/// Build a [`Value`]: `json!({"a": 1, "b": [x, null], "c": some_expr})`.
/// Values are any expression whose type implements [`ToJson`].
#[macro_export]
macro_rules! json {
    // Array elements, one value at a time (nested arrays/objects and `null`
    // first; anything else is an expression).
    (@array [$($done:expr,)*]) => { vec![$($done,)*] };
    (@array [$($done:expr,)*] null $(, $($rest:tt)*)?) => {
        $crate::json!(@array [$($done,)* $crate::json::Value::Null,] $($($rest)*)?)
    };
    (@array [$($done:expr,)*] [$($a:tt)*] $(, $($rest:tt)*)?) => {
        $crate::json!(@array [$($done,)* $crate::json!([$($a)*]),] $($($rest)*)?)
    };
    (@array [$($done:expr,)*] {$($o:tt)*} $(, $($rest:tt)*)?) => {
        $crate::json!(@array [$($done,)* $crate::json!({$($o)*}),] $($($rest)*)?)
    };
    (@array [$($done:expr,)*] $v:expr $(, $($rest:tt)*)?) => {
        $crate::json!(@array [$($done,)* $crate::json!($v),] $($($rest)*)?)
    };

    // Object entries: key tokens up to `:`, then one value.
    (@object $m:ident ()) => {};
    (@object $m:ident ($($k:tt)+) : null $(, $($rest:tt)*)?) => {
        $m.insert(($($k)+).to_string(), $crate::json::Value::Null);
        $crate::json!(@object $m () $($($rest)*)?);
    };
    (@object $m:ident ($($k:tt)+) : [$($a:tt)*] $(, $($rest:tt)*)?) => {
        $m.insert(($($k)+).to_string(), $crate::json!([$($a)*]));
        $crate::json!(@object $m () $($($rest)*)?);
    };
    (@object $m:ident ($($k:tt)+) : {$($o:tt)*} $(, $($rest:tt)*)?) => {
        $m.insert(($($k)+).to_string(), $crate::json!({$($o)*}));
        $crate::json!(@object $m () $($($rest)*)?);
    };
    (@object $m:ident ($($k:tt)+) : $v:expr $(, $($rest:tt)*)?) => {
        $m.insert(($($k)+).to_string(), $crate::json!($v));
        $crate::json!(@object $m () $($($rest)*)?);
    };
    (@object $m:ident ($($k:tt)*) $t:tt $($rest:tt)*) => {
        $crate::json!(@object $m ($($k)* $t) $($rest)*)
    };

    (null) => { $crate::json::Value::Null };
    ([ $($tt:tt)* ]) => { $crate::json::Value::Array($crate::json!(@array [] $($tt)*)) };
    ({ $($tt:tt)* }) => {{
        #[allow(unused_mut)]
        let mut object = $crate::json::Map::new();
        $crate::json!(@object object () $($tt)*);
        $crate::json::Value::Object(object)
    }};
    ($e:expr) => { $crate::json::ToJson::to_json(&$e) };
}

/// [`ToJson`] and [`FromJson`] for a struct, field by field, in order:
///
/// ```ignore
/// json_struct!(Identity { name #default, email #default, agent #default ?Option::is_none });
/// ```
///
/// `#default`: may be missing when read (then `Default::default()`);
/// `@"old"`: also read from field `old`; `?pred`: not written when
/// `pred(&field)` is true. Fields of type `Option` may always be missing.
#[macro_export]
macro_rules! json_struct {
    ($name:ident { $( $f:ident $(#$d:ident)? $(@$alias:literal)? $(?$skip:path)? ),* $(,)? }) => {
        impl $crate::json::ToJson for $name {
            fn write_json(&self, out: &mut String) {
                out.push('{');
                let mut _first = true;
                $(
                    if !( $( $skip(&self.$f) || )? false ) {
                        if !_first {
                            out.push(',');
                        }
                        _first = false;
                        $crate::json::write_str(out, stringify!($f));
                        out.push(':');
                        $crate::json::ToJson::write_json(&self.$f, out);
                    }
                )*
                out.push('}');
            }
        }

        impl $crate::json::FromJson for $name {
            fn from_json(v: &$crate::json::Value) -> $crate::error::Result<Self> {
                let o = v.as_object().ok_or_else(|| $crate::err!("expected an object ({})", stringify!($name)))?;
                Ok($name {
                    $( $f: {
                        let got = o.get(stringify!($f)) $( .or_else(|| o.get($alias)) )?;
                        match got {
                            Some(x) => $crate::json::FromJson::from_json(x)
                                .map_err(|e| e.context(format!("field `{}`", stringify!($f))))?,
                            None => $crate::__json_missing!($($d)? ; stringify!($f)),
                        }
                    }, )*
                })
            }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __json_missing {
    (default ; $n:expr) => {
        Default::default()
    };
    ( ; $n:expr) => {
        $crate::json::FromJson::missing().ok_or_else(|| $crate::err!("missing field `{}`", $n))?
    };
}

/// [`ToJson`] and [`FromJson`] for a fieldless enum, as strings:
/// `json_enum!(TaskStatus { Open = "open", InProgress = "in_progress" });`
#[macro_export]
macro_rules! json_enum {
    ($name:ident { $( $v:ident = $s:literal ),* $(,)? }) => {
        impl $crate::json::ToJson for $name {
            fn write_json(&self, out: &mut String) {
                let s = match self { $( $name::$v => $s, )* };
                $crate::json::write_str(out, s);
            }
        }

        impl $crate::json::FromJson for $name {
            fn from_json(v: &$crate::json::Value) -> $crate::error::Result<Self> {
                match v.as_str() {
                    $( Some($s) => Ok($name::$v), )*
                    Some(other) => Err($crate::err!("unknown variant `{}` of {}", other, stringify!($name))),
                    None => Err($crate::err!("expected a string ({})", stringify!($name))),
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Default)]
    struct Inner {
        a: u64,
        tags: Vec<String>,
    }
    crate::json_struct!(Inner { a, tags #default ?Vec::is_empty });

    #[derive(Debug, PartialEq)]
    struct Outer {
        name: String,
        inner: Inner,
        opt: Option<i64>,
        renamed: u32,
    }
    crate::json_struct!(Outer { name, inner #default, opt, renamed #default @"old_name" });

    #[test]
    fn structs_round_trip_in_field_order() {
        let o = Outer { name: "x\"y\n".into(), inner: Inner { a: 7, tags: vec![] }, opt: Some(-3), renamed: 2 };
        let s = to_string(&o).unwrap();
        assert_eq!(s, r#"{"name":"x\"y\n","inner":{"a":7},"opt":-3,"renamed":2}"#);
        assert_eq!(from_str::<Outer>(&s).unwrap(), o);
        // Missing optional/default fields, an alias, unknown fields.
        let o2: Outer = from_str(r#"{"zzz":1,"name":"n","old_name":5}"#).unwrap();
        assert_eq!((o2.opt, o2.renamed, o2.inner.a), (None, 5, 0));
        let e = from_str::<Outer>(r#"{"inner":{"a":1}}"#).unwrap_err();
        assert_eq!(e.to_string(), "missing field `name`");
        assert!(format!("{:#}", from_str::<Outer>(r#"{"name":1}"#).unwrap_err()).contains("field `name`"));
    }

    #[test]
    fn values_and_macro() {
        let x = 5u32;
        let v = json!({ "b": [1, "two", null, x + 1, { "k": true }], "a": -1, "e": {}, "f": [] , "s": format!("{}!", "hi") });
        assert_eq!(v.to_string(), r#"{"a":-1,"b":[1,"two",null,6,{"k":true}],"e":{},"f":[],"s":"hi!"}"#);
        assert_eq!(v["b"][1], "two");
        assert_eq!(v["b"][3], 6);
        assert!(v["missing"]["deeper"].is_null());
        assert_eq!(v.pointer("/b/4/k"), Some(&Value::Bool(true)));
        assert_eq!(parse(&v.to_string()).unwrap(), v);
        let mut m = Value::Null;
        m["k"] = json!([1]);
        assert_eq!(m.to_string(), r#"{"k":[1]}"#);
    }

    #[test]
    fn pretty_matches_the_usual_layout() {
        let v = json!({ "a": [1, 2], "b": {}, "c": { "d": "x,y:{}" } });
        assert_eq!(format!("{v:#}"), "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": {\n    \"d\": \"x,y:{}\"\n  }\n}");
    }

    #[test]
    fn parser_edge_cases() {
        let v = parse(r#" { "s": "\u00e9\ud83d\ude00\t\/", "n": [0, -0, 18446744073709551615, -9223372036854775808, 1.5, 2e3] } "#).unwrap();
        assert_eq!(v["s"], "é😀\t/");
        assert_eq!(v["n"][2].as_u64(), Some(u64::MAX));
        assert_eq!(v["n"][3].as_i64(), Some(i64::MIN));
        assert_eq!(v["n"][4].as_f64(), Some(1.5));
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "01", "1.", "\"\u{1}\"", "nul", "[1] x", "\"\\x\""] {
            assert!(parse(bad).is_err(), "{bad:?} should fail");
        }
        assert!(parse(&"[".repeat(200)).unwrap_err().to_string().contains("recursion limit"));
        // Control characters are escaped on output and read back.
        let s = "a\u{1}b\u{7f}é";
        assert_eq!(to_string(s).unwrap(), "\"a\\u0001b\u{7f}é\"");
        assert_eq!(from_str::<String>(&to_string(s).unwrap()).unwrap(), s);
    }
}
