use core::{fmt, marker::PhantomData, ops::Range};

use fallible_iterator::FallibleIterator;
use postgres_protocol::message::backend::DataRowBody;
use postgres_types::FromSql;
use xitca_io::bytes::Bytes;

use crate::{column::Column, error::Error, from_sql::FromSqlExt, Type};

use super::traits::RowIndexAndType;

/// A row of data returned from the database by a query.
pub type Row<'r> = GenericRow<'r, marker::Typed>;

/// A row of data returned from the database by a simple query.
pub type RowSimple<'r> = GenericRow<'r, marker::NoTyped>;

/// Marker types for specialized impl on [GenericRow].
mod marker {
    pub struct Typed;
    pub struct NoTyped;
}

pub struct GenericRow<'a, M> {
    columns: &'a [Column],
    body: DataRowBody,
    ranges: &'a mut Vec<Option<Range<usize>>>,
    _marker: PhantomData<M>,
}

impl<M> fmt::Debug for GenericRow<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Row").field("columns", &self.columns()).finish()
    }
}

impl<'a, C> GenericRow<'a, C> {
    pub(crate) fn try_new(
        columns: &'a [Column],
        body: DataRowBody,
        ranges: &'a mut Vec<Option<Range<usize>>>,
    ) -> Result<Self, Error> {
        let mut iter = body.ranges().enumerate();
        ranges.reserve(iter.size_hint().0);
        while let Some((idx, range)) = iter.next()? {
            match ranges.get_mut(idx) {
                Some(r) => *r = range,
                None => ranges.push(range),
            }
        }
        Ok(Self {
            columns,
            body,
            ranges,
            _marker: PhantomData,
        })
    }

    /// Returns information about the columns of data in the row.
    #[inline]
    pub fn columns(&self) -> &[Column] {
        self.columns
    }

    /// Determines if the row contains no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number of values in the row.
    pub fn len(&self) -> usize {
        self.columns().len()
    }

    // Get the raw bytes for the column at the given index.
    fn col_buffer(&self, idx: usize) -> Option<(&Range<usize>, &Bytes)> {
        self.ranges[idx].as_ref().map(|r| (r, self.body.buffer_bytes()))
    }

    fn get_idx_ty(
        &self,
        idx: impl RowIndexAndType + fmt::Display,
        ty_check: impl FnOnce(&Type) -> bool,
    ) -> Result<(usize, &Type), Error> {
        let (idx, ty) = idx
            ._from_columns(self.columns())
            .ok_or_else(|| Error::InvalidColumnIndex(format!("{idx}")))?;

        if !ty_check(ty) {
            return Err(Error::ToDo);
            // return Err(Error::from_sql(Box::new(WrongType::new::<T>(ty.clone())), idx));
        }

        Ok((idx, ty))
    }
}

impl Row<'_> {
    /// Deserializes a value from the row.
    ///
    /// The value can be specified either by its numeric index in the row, or by its column name.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds or if the value cannot be converted to the specified type.
    #[inline]
    pub fn get<'s, T>(&'s self, idx: impl RowIndexAndType + fmt::Display) -> T
    where
        T: FromSqlExt<'s>,
    {
        self.try_get(&idx)
            .unwrap_or_else(|e| panic!("error retrieving column {idx}: {e}"))
    }

    /// Like `Row::get`, but returns a `Result` rather than panicking.
    pub fn try_get<'s, T>(&'s self, idx: impl RowIndexAndType + fmt::Display) -> Result<T, Error>
    where
        T: FromSqlExt<'s>,
    {
        let (idx, ty) = self.get_idx_ty(idx, T::accepts)?;
        FromSqlExt::from_sql_nullable_ext(ty, self.col_buffer(idx)).map_err(Into::into)
    }

    #[doc(hidden)]
    /// hidden api for get row data with [FromSql] trait implementation.
    pub fn get_raw<'s, T>(&'s self, idx: impl RowIndexAndType + fmt::Display) -> T
    where
        T: FromSql<'s>,
    {
        self.try_get_raw(&idx)
            .unwrap_or_else(|e| panic!("error retrieving column {idx}: {e}"))
    }

    #[doc(hidden)]
    /// hidden api for get row data with [FromSql] trait implementation.
    pub fn try_get_raw<'s, T>(&'s self, idx: impl RowIndexAndType + fmt::Display) -> Result<T, Error>
    where
        T: FromSql<'s>,
    {
        let (idx, ty) = self.get_idx_ty(idx, T::accepts)?;
        FromSql::from_sql_nullable(
            ty,
            self.ranges[idx].as_ref().map(|r| &self.body.buffer()[r.start..r.end]),
        )
        .map_err(Into::into)
    }
}

impl RowSimple<'_> {
    /// Returns a value from the row.
    ///
    /// The value can be specified either by its numeric index in the row, or by its column name.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds or if the value cannot be converted to the specified type.
    pub fn get(&self, idx: impl RowIndexAndType + fmt::Display) -> Option<&str> {
        self.try_get(&idx)
            .unwrap_or_else(|e| panic!("error retrieving column {idx}: {e}"))
    }

    /// Like `RowSimple::get`, but returns a `Result` rather than panicking.
    pub fn try_get(&self, idx: impl RowIndexAndType + fmt::Display) -> Result<Option<&str>, Error> {
        let (idx, ty) = self.get_idx_ty(idx, <&str as FromSqlExt>::accepts)?;
        FromSqlExt::from_sql_nullable_ext(ty, self.col_buffer(idx)).map_err(Into::into)
    }
}
