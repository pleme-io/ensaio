//! Generic spec loader — thin wrapper around `tatara_lisp::compile_typed`.

use tatara_lisp::TataraDomain;

use crate::EnsaioError;

/// Compile the entire `src` and return every top-level form of type `T`.
///
/// # Errors
///
/// Returns an error if the source fails to read, macroexpand, or
/// compile under `T`'s schema.
pub fn load_all<T: TataraDomain>(src: &str) -> Result<Vec<T>, EnsaioError> {
    Ok(tatara_lisp::compile_typed::<T>(src)?)
}

/// Convenience: compile `src` and return the single top-level form of
/// type `T`, erroring if there are zero forms.
///
/// # Errors
///
/// Returns an error if compilation fails or produces no forms.
pub fn load_one<T: TataraDomain>(src: &str) -> Result<T, EnsaioError> {
    let mut forms = load_all::<T>(src)?;
    forms
        .pop()
        .ok_or_else(|| EnsaioError::Load(format!("no `({} ...)` form found in source", T::KEYWORD)))
}
