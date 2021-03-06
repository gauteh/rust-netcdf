//! Variables in the netcdf file

#![allow(clippy::similar_names)]
use super::attribute::AttrValue;
use super::attribute::Attribute;
use super::dimension::Dimension;
use super::error;
use super::LOCK;
#[cfg(feature = "ndarray")]
use ndarray::ArrayD;
use netcdf_sys::*;
use std::convert::TryInto;
use std::ffi::CStr;
use std::marker::Sized;

#[allow(clippy::doc_markdown)]
/// This struct defines a netCDF variable.
#[derive(Debug)]
pub struct Variable {
    /// The variable name
    pub(crate) name: String,
    pub(crate) dimensions: Vec<Dimension>,
    /// the netcdf variable type identifier (from netcdf-sys)
    pub(crate) vartype: nc_type,
    pub(crate) ncid: nc_type,
    pub(crate) varid: nc_type,
}

/// Enum for variables endianness
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Endianness {
    /// Native endianness, depends on machine architecture (x86_64 is Little)
    Native,
    /// Lille endian
    Little,
    /// Big endian
    Big,
}

#[allow(clippy::len_without_is_empty)]
impl Variable {
    /// Get name of variable
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Get an attribute of this variable
    pub fn attribute<'a>(&'a self, name: &str) -> error::Result<Option<Attribute<'a>>> {
        // Need to lock when reading the first attribute (per variable)
        let _l = super::LOCK.lock().unwrap();
        Attribute::find_from_name(self.ncid, Some(self.varid), name)
    }
    /// Iterator over all the attributes of this variable
    pub fn attributes<'a>(
        &'a self,
    ) -> error::Result<impl Iterator<Item = error::Result<Attribute<'a>>>> {
        // Need to lock when reading the first attribute (per variable)
        let _l = super::LOCK.lock().unwrap();
        crate::attribute::AttributeIterator::new(self.ncid, Some(self.varid))
    }
    /// Dimensions for a variable
    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }
    /// Get the type of this variable. This will be an integer
    /// such as `NC_FLOAT`, `NC_DOUBLE`, `NC_INT` from
    /// the `netcdf-sys` crate
    pub fn vartype(&self) -> nc_type {
        self.vartype
    }
    /// Get current length of the variable
    pub fn len(&self) -> usize {
        self.dimensions.iter().map(Dimension::len).product()
    }
    /// Get endianness of the variable.
    pub fn endian_value(&self) -> error::Result<Endianness> {
        let mut e: nc_type = 0;
        unsafe {
            error::checked(nc_inq_var_endian(self.ncid, self.varid, &mut e))?;
        }
        match e {
            NC_ENDIAN_NATIVE => Ok(Endianness::Native),
            NC_ENDIAN_LITTLE => Ok(Endianness::Little),
            NC_ENDIAN_BIG => Ok(Endianness::Big),
            _ => Err(NC_EVARMETA.into()),
        }
    }
    /// Set endianness of the variable. Must be set before inserting data
    ///
    /// `endian` can take a `Endianness` value with Native being `NC_ENDIAN_NATIVE` (0),
    /// Little `NC_ENDIAN_LITTLE` (1), Big `NC_ENDIAN_BIG` (2)
    pub fn endian(&mut self, e: Endianness) -> error::Result<()> {
        let _l = LOCK.lock().unwrap();
        let endianness = match e {
            Endianness::Native => NC_ENDIAN_NATIVE,
            Endianness::Little => NC_ENDIAN_LITTLE,
            Endianness::Big => NC_ENDIAN_BIG,
        };
        unsafe {
            error::checked(nc_def_var_endian(self.ncid, self.varid, endianness))?;
        }
        Ok(())
    }

    /// Sets compression on the variable. Must be set before filling in data.
    ///
    /// `deflate_level` can take a value 0..=9, with 0 being no
    /// compression (good for CPU bound tasks), and 9 providing the
    /// highest compression level (good for memory bound tasks)
    pub fn compression(&mut self, deflate_level: nc_type) -> error::Result<()> {
        let _l = LOCK.lock().unwrap();
        unsafe {
            error::checked(nc_def_var_deflate(
                self.ncid,
                self.varid,
                false as _,
                true as _,
                deflate_level,
            ))?;
        }

        Ok(())
    }

    /// Set chunking for variable. Must be set before inserting data
    ///
    /// Use this when reading or writing smaller units of the hypercube than
    /// the full dimensions lengths, to change how the variable is stored in
    /// the file. This has no effect on the memory order when reading/putting
    /// a buffer.
    pub fn chunking(&mut self, chunksize: &[usize]) -> error::Result<()> {
        let _l = LOCK.lock().unwrap();
        if chunksize.len() != self.dimensions.len() {
            return Err(error::Error::SliceLen);
        }
        let len = chunksize
            .iter()
            .fold(1_usize, |acc, &x| acc.saturating_mul(x));
        if len == usize::max_value() {
            return Err(error::Error::Overflow);
        }
        unsafe {
            error::checked(nc_def_var_chunking(
                self.ncid,
                self.varid,
                NC_CHUNKED,
                chunksize.as_ptr(),
            ))?;
        }

        Ok(())
    }

    /// Checks for array mismatch
    fn check_indices(&self, indices: &[usize], putting: bool) -> error::Result<()> {
        if indices.len() != self.dimensions.len() {
            return Err(error::Error::IndexLen);
        }

        for (d, i) in self.dimensions.iter().zip(indices) {
            if d.is_unlimited() && putting {
                continue;
            }
            if *i > d.len() {
                return Err(error::Error::IndexMismatch);
            }
        }

        Ok(())
    }
    /// Create a default [0, 0, ..., 0] offset
    fn default_indices(&self, putting: bool) -> error::Result<Vec<usize>> {
        self.dimensions
            .iter()
            .map(|d| {
                if d.len() > 0 || putting {
                    Ok(0)
                } else {
                    Err(error::Error::IndexMismatch)
                }
            })
            .collect()
    }

    /// Assumes indices is valid for this variable
    fn check_sizelen(
        &self,
        totallen: usize,
        indices: &[usize],
        sizelen: &[usize],
        putting: bool,
    ) -> error::Result<()> {
        if sizelen.len() != self.dimensions.len() {
            return Err(error::Error::SliceLen);
        }

        for ((i, s), d) in indices.iter().zip(sizelen).zip(&self.dimensions) {
            if *s == 0 {
                return Err(error::Error::ZeroSlice);
            }
            if i.checked_add(*s).is_none() {
                return Err(error::Error::Overflow);
            }
            if i + s > d.len() {
                if !putting {
                    return Err(error::Error::SliceMismatch);
                }
                if !d.is_unlimited() {
                    return Err(error::Error::SliceMismatch);
                }
            }
        }

        let thislen = sizelen
            .iter()
            .fold(1_usize, |acc, &x| acc.saturating_mul(x));
        if thislen == usize::max_value() {
            return Err(error::Error::Overflow);
        }

        if totallen != thislen {
            return Err(error::Error::BufferLen(totallen, thislen));
        }

        Ok(())
    }

    /// Assumes indices is valid for this variable
    fn default_sizelen(
        &self,
        totallen: usize,
        indices: &[usize],
        putting: bool,
    ) -> error::Result<Vec<usize>> {
        let num_unlims = self
            .dimensions
            .iter()
            .fold(0, |acc, x| acc + x.is_unlimited() as usize);
        if num_unlims > 1 {
            return Err(error::Error::Ambiguous);
        }

        let mut sizelen = Vec::with_capacity(self.dimensions.len());

        let mut unlim_pos = None;
        for (pos, (&i, d)) in indices.iter().zip(&self.dimensions).enumerate() {
            if i >= d.len() {
                if !d.is_unlimited() {
                    return Err(error::Error::SliceMismatch);
                }
                if !putting {
                    return Err(error::Error::SliceMismatch);
                }
                unlim_pos = Some(pos);
                sizelen.push(1);
            } else if putting && d.is_unlimited() {
                unlim_pos = Some(pos);
                sizelen.push(1);
            } else {
                sizelen.push(d.len() - i);
            }
        }

        if let Some(pos) = unlim_pos {
            let l = sizelen
                .iter()
                .fold(1_usize, |acc, &x| acc.saturating_mul(x));
            if l == usize::max_value() {
                return Err(error::Error::Overflow);
            }
            sizelen[pos] = totallen / l;
        }

        let wantedlen = sizelen
            .iter()
            .fold(1_usize, |acc, &x| acc.saturating_mul(x));
        if wantedlen == usize::max_value() {
            return Err(error::Error::Overflow);
        }
        if totallen != wantedlen {
            return Err(error::Error::BufferLen(totallen, wantedlen));
        }
        Ok(sizelen)
    }
}

#[allow(clippy::doc_markdown)]
/// This trait allow an implicit cast when fetching
/// a netCDF variable. These methods are not be called
/// directly, but used through methods on `Variable`
pub unsafe trait Numeric
where
    Self: Sized,
{
    /// Constant corresponding to a netcdf type
    const NCTYPE: nc_type;

    /// Returns a single indexed value of the variable as Self
    ///
    /// # Safety
    ///
    /// Requires `indices` to be of a valid length
    unsafe fn single_value_from_variable(
        variable: &Variable,
        indices: &[usize],
    ) -> error::Result<Self>;

    /// Get multiple values at once, without checking the validity of
    /// `indices` or `slice_len`
    ///
    /// # Safety
    ///
    /// Requires `values` to be of at least size `slice_len.product()`,
    /// `indices` and `slice_len` to be of a valid length
    unsafe fn variable_to_ptr(
        variable: &Variable,
        indices: &[usize],
        slice_len: &[usize],
        values: *mut Self,
    ) -> error::Result<()>;

    #[allow(clippy::doc_markdown)]
    /// Put a single value into a netCDF variable
    ///
    /// # Safety
    ///
    /// Requires `indices` to be of a valid length
    unsafe fn put_value_at(
        variable: &mut Variable,
        indices: &[usize],
        value: Self,
    ) -> error::Result<()>;

    #[allow(clippy::doc_markdown)]
    /// put a SLICE of values into a netCDF variable at the given index
    ///
    /// # Safety
    ///
    /// Requires `indices` and `slice_len` to be of a valid length
    unsafe fn put_values_at(
        variable: &mut Variable,
        indices: &[usize],
        slice_len: &[usize],
        values: &[Self],
    ) -> error::Result<()>;

    /// get a SLICE of values into the variable, with the source
    /// strided by `strides`
    unsafe fn get_values_strided(
        variable: &Variable,
        indices: &[usize],
        slice_len: &[usize],
        strides: &[isize],
        values: *mut Self,
    ) -> error::Result<()>;

    /// put a SLICE of values into the variable, with the destination
    /// strided by `strides`
    unsafe fn put_values_strided(
        variable: &mut Variable,
        indices: &[usize],
        slice_len: &[usize],
        strides: &[isize],
        values: *const Self,
    ) -> error::Result<()>;
}

#[allow(clippy::doc_markdown)]
/// This macro implements the trait Numeric for the type `sized_type`.
///
/// The use of this macro reduce code duplication for the implementation of Numeric
/// for the common numeric types (i32, f32 ...): they only differs by the name of the
/// C function used to fetch values from the NetCDF variable (eg: `nc_get_var_ushort`, ...).
macro_rules! impl_numeric {
    (
        $sized_type: ty,
        $nc_type: ident,
        $nc_get_var: ident,
        $nc_get_vara_type: ident,
        $nc_get_var1_type: ident,
        $nc_put_var1_type: ident,
        $nc_put_vara_type: ident,
        $nc_get_vars_type: ident,
        $nc_put_vars_type: ident,
    ) => {
        #[allow(clippy::use_self)] // False positives
        unsafe impl Numeric for $sized_type {
            const NCTYPE: nc_type = $nc_type;

            // fetch ONE value from variable using `$nc_get_var1`
            unsafe fn single_value_from_variable(
                variable: &Variable,
                indices: &[usize],
            ) -> error::Result<Self> {
                // initialize `buff` to 0
                let mut buff: Self = 0 as _;
                // Get a pointer to an array
                let indices_ptr = indices.as_ptr();
                let _g = LOCK.lock().unwrap();
                error::checked($nc_get_var1_type(
                    variable.ncid,
                    variable.varid,
                    indices_ptr,
                    &mut buff,
                ))?;
                Ok(buff)
            }

            unsafe fn variable_to_ptr(
                variable: &Variable,
                indices: &[usize],
                slice_len: &[usize],
                values: *mut Self,
            ) -> error::Result<()> {
                let _l = LOCK.lock().unwrap();

                error::checked($nc_get_vara_type(
                    variable.ncid,
                    variable.varid,
                    indices.as_ptr(),
                    slice_len.as_ptr(),
                    values,
                ))
            }

            // put a SINGLE value into a netCDF variable at the given index
            unsafe fn put_value_at(
                variable: &mut Variable,
                indices: &[usize],
                value: Self,
            ) -> error::Result<()> {
                let _g = LOCK.lock().unwrap();
                error::checked($nc_put_var1_type(
                    variable.ncid,
                    variable.varid,
                    indices.as_ptr(),
                    &value,
                ))
            }

            // put a SLICE of values into a netCDF variable at the given index
            unsafe fn put_values_at(
                variable: &mut Variable,
                indices: &[usize],
                slice_len: &[usize],
                values: &[Self],
            ) -> error::Result<()> {
                let _l = LOCK.lock().unwrap();
                error::checked($nc_put_vara_type(
                    variable.ncid,
                    variable.varid,
                    indices.as_ptr(),
                    slice_len.as_ptr(),
                    values.as_ptr(),
                ))
            }

            unsafe fn get_values_strided(
                variable: &Variable,
                indices: &[usize],
                slice_len: &[usize],
                strides: &[isize],
                values: *mut Self,
            ) -> error::Result<()> {
                let _l = LOCK.lock().unwrap();
                error::checked($nc_get_vars_type(
                    variable.ncid,
                    variable.varid,
                    indices.as_ptr(),
                    slice_len.as_ptr(),
                    strides.as_ptr(),
                    values,
                ))
            }

            unsafe fn put_values_strided(
                variable: &mut Variable,
                indices: &[usize],
                slice_len: &[usize],
                strides: &[isize],
                values: *const Self,
            ) -> error::Result<()> {
                let _l = LOCK.lock().unwrap();
                error::checked($nc_put_vars_type(
                    variable.ncid,
                    variable.varid,
                    indices.as_ptr(),
                    slice_len.as_ptr(),
                    strides.as_ptr(),
                    values,
                ))
            }
        }
    };
}
impl_numeric!(
    u8,
    NC_UBYTE,
    nc_get_var_uchar,
    nc_get_vara_uchar,
    nc_get_var1_uchar,
    nc_put_var1_uchar,
    nc_put_vara_uchar,
    nc_get_vars_uchar,
    nc_put_vars_uchar,
);

impl_numeric!(
    i8,
    NC_BYTE,
    nc_get_var_schar,
    nc_get_vara_schar,
    nc_get_var1_schar,
    nc_put_var1_schar,
    nc_put_vara_schar,
    nc_get_vars_schar,
    nc_put_vars_schar,
);

impl_numeric!(
    i16,
    NC_SHORT,
    nc_get_var_short,
    nc_get_vara_short,
    nc_get_var1_short,
    nc_put_var1_short,
    nc_put_vara_short,
    nc_get_vars_short,
    nc_put_vars_short,
);

impl_numeric!(
    u16,
    NC_USHORT,
    nc_get_var_ushort,
    nc_get_vara_ushort,
    nc_get_var1_ushort,
    nc_put_var1_ushort,
    nc_put_vara_ushort,
    nc_get_vars_ushort,
    nc_put_vars_ushort,
);

impl_numeric!(
    i32,
    NC_INT,
    nc_get_var_int,
    nc_get_vara_int,
    nc_get_var1_int,
    nc_put_var1_int,
    nc_put_vara_int,
    nc_get_vars_int,
    nc_put_vars_int,
);

impl_numeric!(
    u32,
    NC_UINT,
    nc_get_var_uint,
    nc_get_vara_uint,
    nc_get_var1_uint,
    nc_put_var1_uint,
    nc_put_vara_uint,
    nc_get_vars_uint,
    nc_put_vars_uint,
);

impl_numeric!(
    i64,
    NC_INT64,
    nc_get_var_longlong,
    nc_get_vara_longlong,
    nc_get_var1_longlong,
    nc_put_var1_longlong,
    nc_put_vara_longlong,
    nc_get_vars_longlong,
    nc_put_vars_longlong,
);

impl_numeric!(
    u64,
    NC_UINT64,
    nc_get_var_ulonglong,
    nc_get_vara_ulonglong,
    nc_get_var1_ulonglong,
    nc_put_var1_ulonglong,
    nc_put_vara_ulonglong,
    nc_get_vars_ulonglong,
    nc_put_vars_ulonglong,
);

impl_numeric!(
    f32,
    NC_FLOAT,
    nc_get_var_float,
    nc_get_vara_float,
    nc_get_var1_float,
    nc_put_var1_float,
    nc_put_vara_float,
    nc_get_vars_float,
    nc_put_vars_float,
);

impl_numeric!(
    f64,
    NC_DOUBLE,
    nc_get_var_double,
    nc_get_vara_double,
    nc_get_var1_double,
    nc_put_var1_double,
    nc_put_vara_double,
    nc_get_vars_double,
    nc_put_vars_double,
);

/// Holds the contents of a netcdf string. Use deref to get a `CStr`
struct NcString {
    data: *mut std::os::raw::c_char,
}
impl NcString {
    /// Create an `NcString`
    unsafe fn from_ptr(ptr: *mut i8) -> Self {
        Self { data: ptr }
    }
}
impl Drop for NcString {
    fn drop(&mut self) {
        unsafe {
            error::checked(nc_free_string(1, &mut self.data)).unwrap();
        }
    }
}
impl std::ops::Deref for NcString {
    type Target = CStr;
    fn deref(&self) -> &Self::Target {
        unsafe { CStr::from_ptr(self.data) }
    }
}

impl Variable {
    pub(crate) fn new(
        grp_id: nc_type,
        name: &str,
        dims: Vec<Dimension>,
        vartype: nc_type,
    ) -> error::Result<Self> {
        use std::ffi::CString;
        let cname = CString::new(name).unwrap();

        let dimids: Vec<nc_type> = dims.iter().map(|x| x.id).collect();
        let mut id = 0;
        unsafe {
            let _l = LOCK.lock().unwrap();
            error::checked(nc_def_var(
                grp_id,
                cname.as_ptr(),
                vartype,
                dimids.len().try_into()?,
                dimids.as_ptr(),
                &mut id,
            ))?;
        }

        Ok(Self {
            name: name.into(),
            dimensions: dims,
            vartype,
            ncid: grp_id,
            varid: id,
        })
    }

    /// Adds an attribute to the variable
    pub fn add_attribute<T>(&mut self, name: &str, val: T) -> error::Result<Attribute>
    where
        T: Into<AttrValue>,
    {
        Attribute::put(self.ncid, self.varid, name, val.into())
    }

    ///  Fetches one specific value at specific indices
    ///  indices must has the same length as self.dimensions.
    pub fn value<T: Numeric>(&self, indices: Option<&[usize]>) -> error::Result<T> {
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, false)?;
            x
        } else {
            indices_ = self.default_indices(false)?;
            &indices_
        };

        unsafe { T::single_value_from_variable(self, indices) }
    }

    /// Reads a string variable. This involves two copies per read, and should
    /// be avoided in performance critical code
    pub fn string_value(&self, indices: Option<&[usize]>) -> error::Result<String> {
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, false)?;
            x
        } else {
            indices_ = self.default_indices(false)?;
            &indices_
        };
        let _l = LOCK.lock().unwrap();

        let mut s: *mut std::os::raw::c_char = std::ptr::null_mut();
        unsafe {
            error::checked(nc_get_var1_string(
                self.ncid,
                self.varid,
                indices.as_ptr(),
                &mut s,
            ))?;
        }
        let string = unsafe { NcString::from_ptr(s) };
        Ok(string.to_string_lossy().into_owned())
    }

    #[cfg(feature = "ndarray")]
    /// Fetches variable
    pub fn values<T: Numeric>(
        &self,
        indices: Option<&[usize]>,
        slice_len: Option<&[usize]>,
    ) -> error::Result<ArrayD<T>> {
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, false)?;
            x
        } else {
            indices_ = self.default_indices(false)?;
            &indices_
        };
        let slice_len_: Vec<usize>;
        let full_length;
        let slice_len = if let Some(x) = slice_len {
            full_length = x.iter().fold(1_usize, |acc, x| acc.saturating_mul(*x));
            if full_length == usize::max_value() {
                return Err(error::Error::Overflow);
            }
            self.check_sizelen(full_length, indices, x, false)?;
            x
        } else {
            full_length = self.dimensions.iter().map(Dimension::len).product();
            slice_len_ = self.default_sizelen(full_length, indices, false)?;
            &slice_len_
        };

        let mut values = Vec::with_capacity(full_length);
        unsafe {
            T::variable_to_ptr(self, indices, slice_len, values.as_mut_ptr())?;
            values.set_len(full_length);
        }
        Ok(ArrayD::from_shape_vec(slice_len, values).unwrap())
    }

    /// Fetches variable into slice
    /// buffer must be able to hold all the requested elements
    pub fn values_to<T: Numeric>(
        &self,
        buffer: &mut [T],
        indices: Option<&[usize]>,
        slice_len: Option<&[usize]>,
    ) -> error::Result<()> {
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, false)?;
            x
        } else {
            indices_ = self.default_indices(false)?;
            &indices_
        };
        let slice_len_: Vec<usize>;
        let slice_len = if let Some(x) = slice_len {
            self.check_sizelen(buffer.len(), indices, x, false)?;
            x
        } else {
            slice_len_ = self.default_sizelen(buffer.len(), indices, false)?;
            &slice_len_
        };

        unsafe { T::variable_to_ptr(self, indices, slice_len, buffer.as_mut_ptr()) }
    }

    /// Fetches variable into slice
    /// buffer must be able to hold all the requested elements
    pub fn values_strided_to<T: Numeric>(
        &self,
        buffer: &mut [T],
        indices: Option<&[usize]>,
        slice_len: Option<&[usize]>,
        strides: &[isize],
    ) -> error::Result<usize> {
        if strides.len() != self.dimensions.len() {
            return Err("stride_mismatch".into());
        }
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, false)?;
            x
        } else {
            indices_ = self.default_indices(false)?;
            &indices_
        };

        let slice_len_: Vec<usize>;
        let slice_len = if let Some(slice_len) = slice_len {
            if slice_len.len() != self.dimensions.len() {
                return Err("slice mismatch".into());
            }
            for (((d, &start), &count), &stride) in self
                .dimensions
                .iter()
                .zip(indices)
                .zip(slice_len)
                .zip(strides)
            {
                if stride == 0 && count != 1 {
                    return Err(error::Error::StrideError);
                }
                if count == 0 {
                    return Err(error::Error::ZeroSlice);
                }
                if start as isize + (count as isize - 1) * stride > d.len() as isize {
                    return Err(error::Error::IndexMismatch);
                }
                if start as isize + count as isize * stride < 0 {
                    return Err(error::Error::IndexMismatch);
                }
            }
            slice_len
        } else {
            slice_len_ = self
                .dimensions
                .iter()
                .zip(indices)
                .zip(strides)
                .map(|((d, &start), &stride)| {
                    if stride == 0 {
                        1
                    } else if stride < 0 {
                        start / stride.abs() as usize
                    } else {
                        let dlen = d.len();
                        let round_up = stride.abs() as usize - 1;
                        (dlen - start + round_up) / stride.abs() as usize
                    }
                })
                .collect::<Vec<_>>();
            &slice_len_
        };
        if buffer.len() < slice_len.iter().product() {
            return Err("buffer too small".into());
        }
        unsafe { T::get_values_strided(self, indices, &slice_len, strides, buffer.as_mut_ptr())? };
        Ok(slice_len.iter().product())
    }

    /// Put a single value at `indices`
    pub fn put_value<T: Numeric>(
        &mut self,
        value: T,
        indices: Option<&[usize]>,
    ) -> error::Result<()> {
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, true)?;
            x
        } else {
            indices_ = self.default_indices(true)?;
            &indices_
        };
        unsafe { T::put_value_at(self, indices, value) }
    }

    /// Internally converts to a `CString`, avoid using this function when performance
    /// is important
    pub fn put_string(&mut self, value: &str, indices: Option<&[usize]>) -> error::Result<()> {
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, true)?;
            x
        } else {
            indices_ = self.default_indices(true)?;
            &indices_
        };

        let value = std::ffi::CString::new(value).expect("String contained interior 0");
        let mut ptr = value.as_ptr();

        let _l = LOCK.lock().unwrap();

        unsafe {
            error::checked(nc_put_var1_string(
                self.ncid,
                self.varid,
                indices.as_ptr(),
                &mut ptr,
            ))?
        }

        Ok(())
    }

    /// Put a slice of values at `indices`
    pub fn put_values<T: Numeric>(
        &mut self,
        values: &[T],
        indices: Option<&[usize]>,
        slice_len: Option<&[usize]>,
    ) -> error::Result<()> {
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, true)?;
            x
        } else {
            indices_ = self.default_indices(true)?;
            &indices_
        };
        let slice_len_: Vec<usize>;
        let slice_len = if let Some(x) = slice_len {
            self.check_sizelen(values.len(), indices, x, true)?;
            x
        } else {
            slice_len_ = self.default_sizelen(values.len(), indices, true)?;
            &slice_len_
        };
        unsafe { T::put_values_at(self, indices, slice_len, values) }
    }

    /// Put a slice of values at `indices`, with destination strided
    pub fn put_values_strided<T: Numeric>(
        &mut self,
        values: &[T],
        indices: Option<&[usize]>,
        slice_len: Option<&[usize]>,
        strides: &[isize],
    ) -> error::Result<usize> {
        let indices_: Vec<usize>;
        let indices = if let Some(x) = indices {
            self.check_indices(x, true)?;
            x
        } else {
            indices_ = self.default_indices(true)?;
            &indices_
        };
        if strides.len() != self.dimensions.len() {
            return Err(error::Error::IndexMismatch);
        }

        let slice_len_: Vec<usize>;
        let slice_len = if let Some(slice_len) = slice_len {
            if slice_len.len() != self.dimensions.len() {
                return Err(error::Error::SliceMismatch);
            }
            for (((d, &start), &count), &stride) in self
                .dimensions
                .iter()
                .zip(indices)
                .zip(slice_len)
                .zip(strides)
            {
                if count == 0 {
                    return Err(error::Error::ZeroSlice);
                }
                let end = start as isize + (count as isize - 1) * stride;
                if end < 0 {
                    return Err(error::Error::IndexMismatch);
                }
                if !d.is_unlimited() && end > d.len() as isize {
                    return Err(error::Error::IndexMismatch);
                }
            }
            slice_len
        } else {
            slice_len_ = self
                .dimensions
                .iter()
                .zip(indices)
                .zip(strides)
                .map(|((d, &start), &stride)| {
                    match stride {
                        0 => 1,
                        stride if stride > 0 => {
                            let dlen = d.len();
                            let nelems = (dlen - start + stride as usize - 1) / stride as usize;
                            nelems
                        }
                        _stride => {
                            // Negative stride
                            1
                        }
                    }
                })
                .collect();
            &slice_len_
        };
        if values.len() < slice_len.iter().product() {
            return Err("not enough values".into());
        }
        unsafe { T::put_values_strided(self, indices, slice_len, strides, values.as_ptr())? };
        Ok(slice_len.iter().product())
    }

    /// Set a Fill Value
    #[allow(clippy::needless_pass_by_value)] // All values will be small
    pub fn set_fill_value<T>(&mut self, fill_value: T) -> error::Result<()>
    where
        T: Numeric,
    {
        if T::NCTYPE != self.vartype {
            return Err(error::Error::TypeMismatch);
        }
        let _l = LOCK.lock().unwrap();
        unsafe {
            error::checked(nc_def_var_fill(
                self.ncid,
                self.varid,
                NC_FILL,
                &fill_value as *const T as *const _,
            ))?;
        }
        Ok(())
    }

    /// Set the fill value to no value. Use this when wanting to avoid
    /// duplicate writes into empty variables.
    ///
    /// # Safety
    ///
    /// This is unsafe, as reading from this variable without
    /// writing to it will produce potential garbage values
    pub unsafe fn set_nofill(&mut self) -> error::Result<()> {
        let _l = LOCK.lock().unwrap();
        error::checked(nc_def_var_fill(
            self.ncid,
            self.varid,
            NC_NOFILL,
            std::ptr::null_mut(),
        ))
    }

    /// Get the fill value of a variable
    pub fn fill_value<T: Numeric>(&self) -> error::Result<Option<T>> {
        if T::NCTYPE != self.vartype {
            return Err(error::Error::TypeMismatch);
        }
        let mut location = std::mem::MaybeUninit::uninit();
        let mut nofill: nc_type = 0;
        let _l = LOCK.lock().unwrap();
        unsafe {
            error::checked(nc_inq_var_fill(
                self.ncid,
                self.varid,
                &mut nofill,
                &mut location as *mut _ as *mut _,
            ))?;
        }
        if nofill == 1 {
            return Ok(None);
        }

        Ok(Some(unsafe { location.assume_init() }))
    }
}
