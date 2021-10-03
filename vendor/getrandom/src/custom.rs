// Copyright 2018 Developers of the Rand project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! An implementation which calls out to an externally defined function.
use crate::Error;
use core::num::NonZeroU32;

/// Register a function to be invoked by `getrandom` on unsupported targets.
///
/// *This API requires the `"custom"` Cargo feature to be activated*.
///
/// ## Writing a custom `getrandom` implementation
///
/// The function to register must have the same signature as
/// [`getrandom::getrandom`](crate::getrandom). The function can be defined
/// wherever you want, either in root crate or a dependant crate.
///
/// For example, if we wanted a `failure-getrandom` crate containing an
/// implementation that always fails, we would first depend on `getrandom`
/// (for the [`Error`] type) in `failure-getrandom/Cargo.toml`:
/// ```toml
/// [dependencies]
/// getrandom = "0.2"
/// ```
/// Note that the crate containing this function does **not** need to enable the
/// `"custom"` Cargo feature.
///
/// Next, in `failure-getrandom/src/lib.rs`, we define our function:
/// ```rust
/// use core::num::NonZeroU32;
/// use getrandom::Error;
///
/// // Some application-specific error code
/// const MY_CUSTOM_ERROR_CODE: u32 = Error::CUSTOM_START + 42;
/// pub fn always_fail(buf: &mut [u8]) -> Result<(), Error> {
///     let code = NonZeroU32::new(MY_CUSTOM_ERROR_CODE).unwrap();
///     Err(Error::from(code))
/// }
/// ```
///
/// ## Registering a custom `getrandom` implementation
///
/// Functions can only be registered in the root binary crate. Attempting to
/// register a function in a non-root crate will result in a linker error.
/// This is similar to
/// [`#[panic_handler]`](https://doc.rust-lang.org/nomicon/panic-handler.html) or
/// [`#[global_allocator]`](https://doc.rust-lang.org/edition-guide/rust-2018/platform-and-target-support/global-allocators.html),
/// where helper crates define handlers/allocators but only the binary crate
/// actually _uses_ the functionality.
///
/// To register the function, we first depend on `failure-getrandom` _and_
/// `getrandom` in `Cargo.toml`:
/// ```toml
/// [dependencies]
/// failure-getrandom = "0.1"
/// getrandom = { version = "0.2", features = ["custom"] }
/// ```
///
/// Then, we register the function in `src/main.rs`:
/// ```rust
/// # mod failure_getrandom { pub fn always_fail(_: &mut [u8]) -> Result<(), getrandom::Error> { unimplemented!() } }
/// use failure_getrandom::always_fail;
/// use getrandom::register_custom_getrandom;
///
/// register_custom_getrandom!(always_fail);
/// ```
///
/// Now any user of `getrandom` (direct or indirect) on this target will use the
/// registered function. As noted in the
/// [top-level documentation](index.html#use-a-custom-implementation) this
/// registration only has an effect on unsupported targets.
#[macro_export]
macro_rules! register_custom_getrandom {
    ($path:path) => {
        // We use an extern "C" function to get the guarantees of a stable ABI.
        #[no_mangle]
        extern "C" fn __getrandom_custom(dest: *mut u8, len: usize) -> u32 {
            let f: fn(&mut [u8]) -> Result<(), ::getrandom::Error> = $path;
            let slice = unsafe { ::core::slice::from_raw_parts_mut(dest, len) };
            match f(slice) {
                Ok(()) => 0,
                Err(e) => e.code().get(),
            }
        }
    };
}

#[allow(dead_code)]
pub fn getrandom_inner(dest: &mut [u8]) -> Result<(), Error> {
    extern "C" {
        fn __getrandom_custom(dest: *mut u8, len: usize) -> u32;
    }
    let ret = unsafe { __getrandom_custom(dest.as_mut_ptr(), dest.len()) };
    match NonZeroU32::new(ret) {
        None => Ok(()),
        Some(code) => Err(Error::from(code)),
    }
}
