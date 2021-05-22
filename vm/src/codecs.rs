use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;

use crate::builtins::{pybool, PyBytesRef, PyStr, PyStrRef, PyTuple, PyTupleRef};
use crate::common::lock::PyRwLock;
use crate::exceptions::PyBaseExceptionRef;
use crate::VirtualMachine;
use crate::{IntoPyObject, PyContext, PyObjectRef, PyResult, PyValue, TryFromObject, TypeProtocol};

pub struct CodecsRegistry {
    inner: PyRwLock<RegistryInner>,
}

struct RegistryInner {
    search_path: Vec<PyObjectRef>,
    search_cache: HashMap<String, PyCodec>,
    errors: HashMap<String, PyObjectRef>,
}

pub const DEFAULT_ENCODING: &str = "utf-8";

#[derive(Clone)]
#[repr(transparent)]
pub struct PyCodec(PyTupleRef);
impl PyCodec {
    #[inline]
    pub fn from_tuple(tuple: PyTupleRef) -> Result<Self, PyTupleRef> {
        if tuple.len() == 4 {
            Ok(PyCodec(tuple))
        } else {
            Err(tuple)
        }
    }
    #[inline]
    pub fn into_tuple(self) -> PyTupleRef {
        self.0
    }
    #[inline]
    pub fn as_tuple(&self) -> &PyTupleRef {
        &self.0
    }

    #[inline]
    pub fn get_encode_func(&self) -> &PyObjectRef {
        &self.0.as_slice()[0]
    }
    #[inline]
    pub fn get_decode_func(&self) -> &PyObjectRef {
        &self.0.as_slice()[1]
    }

    pub fn is_text_codec(&self, vm: &VirtualMachine) -> PyResult<bool> {
        let is_text = vm.get_attribute_opt(self.0.clone().into_object(), "_is_text_encoding")?;
        is_text.map_or(Ok(true), |is_text| pybool::boolval(vm, is_text))
    }

    pub fn encode(
        &self,
        obj: PyObjectRef,
        errors: Option<PyStrRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let args = match errors {
            Some(errors) => vec![obj, errors.into_object()],
            None => vec![obj],
        };
        let res = vm.invoke(self.get_encode_func(), args)?;
        let res = res
            .downcast::<PyTuple>()
            .ok()
            .filter(|tuple| tuple.len() == 2)
            .ok_or_else(|| {
                vm.new_type_error("encoder must return a tuple (object, integer)".to_owned())
            })?;
        // we don't actually care about the integer
        Ok(res.as_slice()[0].clone())
    }

    pub fn decode(
        &self,
        obj: PyObjectRef,
        errors: Option<PyStrRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let args = match errors {
            Some(errors) => vec![obj, errors.into_object()],
            None => vec![obj],
        };
        let res = vm.invoke(self.get_decode_func(), args)?;
        let res = res
            .downcast::<PyTuple>()
            .ok()
            .filter(|tuple| tuple.len() == 2)
            .ok_or_else(|| {
                vm.new_type_error("decoder must return a tuple (object,integer)".to_owned())
            })?;
        // we don't actually care about the integer
        Ok(res.as_slice()[0].clone())
    }

    pub fn get_incremental_encoder(
        &self,
        errors: Option<PyStrRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let args = match errors {
            Some(e) => vec![e.into_object()],
            None => vec![],
        };
        vm.call_method(self.0.as_object(), "incrementalencoder", args)
    }

    pub fn get_incremental_decoder(
        &self,
        errors: Option<PyStrRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let args = match errors {
            Some(e) => vec![e.into_object()],
            None => vec![],
        };
        vm.call_method(self.0.as_object(), "incrementaldecoder", args)
    }
}

impl TryFromObject for PyCodec {
    fn try_from_object(vm: &VirtualMachine, obj: PyObjectRef) -> PyResult<Self> {
        obj.downcast::<PyTuple>()
            .ok()
            .and_then(|tuple| PyCodec::from_tuple(tuple).ok())
            .ok_or_else(|| {
                vm.new_type_error("codec search functions must return 4-tuples".to_owned())
            })
    }
}

impl IntoPyObject for PyCodec {
    #[inline]
    fn into_pyobject(self, _vm: &VirtualMachine) -> PyObjectRef {
        self.0.into_object()
    }
}

impl CodecsRegistry {
    pub(crate) fn new(ctx: &PyContext) -> Self {
        let errors = [
            ("strict", ctx.new_function("strict_errors", strict_errors)),
            ("ignore", ctx.new_function("ignore_errors", ignore_errors)),
            (
                "replace",
                ctx.new_function("replace_errors", replace_errors),
            ),
            (
                "xmlcharrefreplace",
                ctx.new_function("xmlcharrefreplace_errors", xmlcharrefreplace_errors),
            ),
            (
                "backslashreplace",
                ctx.new_function("backslashreplace_errors", backslashreplace_errors),
            ),
        ];
        let errors = std::array::IntoIter::new(errors)
            .map(|(name, f)| (name.to_owned(), f))
            .collect();
        let inner = RegistryInner {
            search_path: Vec::new(),
            search_cache: HashMap::new(),
            errors,
        };
        CodecsRegistry {
            inner: PyRwLock::new(inner),
        }
    }

    pub fn register(&self, search_function: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        if !vm.is_callable(&search_function) {
            return Err(vm.new_type_error("argument must be callable".to_owned()));
        }
        self.inner.write().search_path.push(search_function);
        Ok(())
    }

    pub fn lookup(&self, encoding: &str, vm: &VirtualMachine) -> PyResult<PyCodec> {
        let encoding = normalize_encoding_name(encoding);
        let inner = self.inner.read();
        if let Some(codec) = inner.search_cache.get(encoding.as_ref()) {
            return Ok(codec.clone());
        }
        let search_path = inner.search_path.clone();
        drop(inner); // don't want to deadlock
        let encoding = PyStr::from(encoding.into_owned()).into_ref(vm);
        for func in search_path {
            let res = vm.invoke(&func, (encoding.clone(),))?;
            let res = <Option<PyCodec>>::try_from_object(vm, res)?;
            if let Some(codec) = res {
                let mut inner = self.inner.write();
                // someone might have raced us to this, so use theirs
                let codec = inner
                    .search_cache
                    .entry(encoding.as_str().to_owned())
                    .or_insert(codec);
                return Ok(codec.clone());
            }
        }
        Err(vm.new_lookup_error(format!("unknown encoding: {}", encoding)))
    }

    fn _lookup_text_encoding(
        &self,
        encoding: &str,
        generic_func: &str,
        vm: &VirtualMachine,
    ) -> PyResult<PyCodec> {
        let codec = self.lookup(encoding, vm)?;
        if codec.is_text_codec(vm)? {
            Ok(codec)
        } else {
            Err(vm.new_lookup_error(format!(
                "'{}' is not a text encoding; use {} to handle arbitrary codecs",
                encoding, generic_func
            )))
        }
    }

    pub fn forget(&self, encoding: &str) -> Option<PyCodec> {
        let encoding = normalize_encoding_name(encoding);
        self.inner.write().search_cache.remove(encoding.as_ref())
    }

    pub fn encode(
        &self,
        obj: PyObjectRef,
        encoding: &str,
        errors: Option<PyStrRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let codec = self.lookup(encoding, vm)?;
        codec.encode(obj, errors, vm)
    }

    pub fn decode(
        &self,
        obj: PyObjectRef,
        encoding: &str,
        errors: Option<PyStrRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let codec = self.lookup(encoding, vm)?;
        codec.decode(obj, errors, vm)
    }

    pub fn encode_text(
        &self,
        obj: PyStrRef,
        encoding: &str,
        errors: Option<PyStrRef>,
        vm: &VirtualMachine,
    ) -> PyResult<PyBytesRef> {
        let codec = self._lookup_text_encoding(encoding, "codecs.encode()", vm)?;
        codec
            .encode(obj.into_object(), errors, vm)?
            .downcast()
            .map_err(|obj| {
                vm.new_type_error(format!(
                    "'{}' encoder returned '{}' instead of 'bytes'; use codecs.encode() to \
                     encode arbitrary types",
                    encoding,
                    obj.class().name,
                ))
            })
    }

    pub fn decode_text(
        &self,
        obj: PyObjectRef,
        encoding: &str,
        errors: Option<PyStrRef>,
        vm: &VirtualMachine,
    ) -> PyResult<PyStrRef> {
        let codec = self._lookup_text_encoding(encoding, "codecs.decode()", vm)?;
        codec.decode(obj, errors, vm)?.downcast().map_err(|obj| {
            vm.new_type_error(format!(
                "'{}' decoder returned '{}' instead of 'str'; use codecs.decode() \
                 to encode arbitrary types",
                encoding,
                obj.class().name,
            ))
        })
    }

    pub fn register_error(&self, name: String, handler: PyObjectRef) -> Option<PyObjectRef> {
        self.inner.write().errors.insert(name, handler)
    }

    pub fn lookup_error_opt(&self, name: &str) -> Option<PyObjectRef> {
        self.inner.read().errors.get(name).cloned()
    }

    pub fn lookup_error(&self, name: &str, vm: &VirtualMachine) -> PyResult<PyObjectRef> {
        self.lookup_error_opt(name)
            .ok_or_else(|| vm.new_lookup_error(format!("unknown error handler name '{}'", name)))
    }
}

fn normalize_encoding_name(encoding: &str) -> Cow<'_, str> {
    if let Some(i) = encoding.find(|c: char| c == ' ' || c.is_ascii_uppercase()) {
        let mut out = encoding.as_bytes().to_owned();
        for byte in &mut out[i..] {
            if *byte == b' ' {
                *byte = b'-';
            } else {
                byte.make_ascii_lowercase();
            }
        }
        String::from_utf8(out).unwrap().into()
    } else {
        encoding.into()
    }
}

// TODO: exceptions with custom payloads
fn extract_unicode_error_range(err: &PyObjectRef, vm: &VirtualMachine) -> PyResult<Range<usize>> {
    let start = vm.get_attribute(err.clone(), "start")?;
    let start = usize::try_from_object(vm, start)?;
    let end = vm.get_attribute(err.clone(), "end")?;
    let end = usize::try_from_object(vm, end)?;
    Ok(Range { start, end })
}

#[inline]
fn is_decode_err(err: &PyObjectRef, vm: &VirtualMachine) -> bool {
    err.isinstance(&vm.ctx.exceptions.unicode_decode_error)
}
#[inline]
fn is_encode_ish_err(err: &PyObjectRef, vm: &VirtualMachine) -> bool {
    err.isinstance(&vm.ctx.exceptions.unicode_encode_error)
        || err.isinstance(&vm.ctx.exceptions.unicode_translate_error)
}

fn bad_err_type(err: PyObjectRef, vm: &VirtualMachine) -> PyBaseExceptionRef {
    vm.new_type_error(format!(
        "don't know how to handle {} in error callback",
        err.class().name
    ))
}

fn strict_errors(err: PyObjectRef, vm: &VirtualMachine) -> PyResult {
    let err = err
        .downcast()
        .unwrap_or_else(|_| vm.new_type_error("codec must pass exception instance".to_owned()));
    Err(err)
}

fn ignore_errors(err: PyObjectRef, vm: &VirtualMachine) -> PyResult<(PyObjectRef, usize)> {
    if is_encode_ish_err(&err, vm) || is_decode_err(&err, vm) {
        let range = extract_unicode_error_range(&err, vm)?;
        Ok((vm.ctx.new_str(""), range.end))
    } else {
        Err(bad_err_type(err, vm))
    }
}

fn replace_errors(err: PyObjectRef, vm: &VirtualMachine) -> PyResult<(String, usize)> {
    // char::REPLACEMENT_CHARACTER as a str
    let replacement_char = "\u{FFFD}";
    let replace = if err.isinstance(&vm.ctx.exceptions.unicode_encode_error) {
        "?"
    } else if err.isinstance(&vm.ctx.exceptions.unicode_decode_error) {
        let range = extract_unicode_error_range(&err, vm)?;
        return Ok((replacement_char.to_owned(), range.end));
    } else if err.isinstance(&vm.ctx.exceptions.unicode_translate_error) {
        replacement_char
    } else {
        return Err(bad_err_type(err, vm));
    };
    let range = extract_unicode_error_range(&err, vm)?;
    let replace = replace.repeat(range.end - range.start);
    Ok((replace, range.end))
}

fn xmlcharrefreplace_errors(err: PyObjectRef, vm: &VirtualMachine) -> PyResult<(String, usize)> {
    if !is_encode_ish_err(&err, vm) {
        return Err(bad_err_type(err, vm));
    }
    let range = extract_unicode_error_range(&err, vm)?;
    let s = PyStrRef::try_from_object(vm, vm.get_attribute(err, "object")?)?;
    let s_after_start = crate::common::str::try_get_chars(s.as_str(), range.start..).unwrap_or("");
    let num_chars = range.len();
    // capacity rough guess; assuming that the codepoints are 3 digits in decimal + the &#;
    let mut out = String::with_capacity(num_chars * 6);
    for c in s_after_start.chars().take(num_chars) {
        use std::fmt::Write;
        write!(out, "&#{};", c as u32).unwrap()
    }
    Ok((out, range.end))
}

fn backslashreplace_errors(err: PyObjectRef, vm: &VirtualMachine) -> PyResult<(String, usize)> {
    if is_decode_err(&err, vm) {
        let range = extract_unicode_error_range(&err, vm)?;
        let b = PyBytesRef::try_from_object(vm, vm.get_attribute(err, "object")?)?;
        let mut replace = String::with_capacity(4 * range.len());
        for &c in &b[range.clone()] {
            use std::fmt::Write;
            write!(replace, "\\x{:02x}", c).unwrap();
        }
        return Ok((replace, range.end));
    } else if !is_encode_ish_err(&err, vm) {
        return Err(bad_err_type(err, vm));
    }
    let range = extract_unicode_error_range(&err, vm)?;
    let s = PyStrRef::try_from_object(vm, vm.get_attribute(err, "object")?)?;
    let s_after_start = crate::common::str::try_get_chars(s.as_str(), range.start..).unwrap_or("");
    let num_chars = range.len();
    // minimum 4 output bytes per char: \xNN
    let mut out = String::with_capacity(num_chars * 4);
    for c in s_after_start.chars().take(num_chars) {
        use std::fmt::Write;
        let c = c as u32;
        if c >= 0x10000 {
            write!(out, "\\U{:08x}", c).unwrap();
        } else if c >= 0x100 {
            write!(out, "\\u{:04x}", c).unwrap();
        } else {
            write!(out, "\\x{:02x}", c).unwrap();
        }
    }
    Ok((out, range.end))
}
