//! Bindings to [Wolfram Symbolic Transfer Protocol (WSTP)](https://www.wolfram.com/wstp/).
//!
//! This crate provides a set of safe and ergonomic bindings to the WSTP library, used to
//! transfer Wolfram Language expressions between programs.

mod env;
mod error;
mod link_server;
mod wait;

mod get;
mod put;


use std::convert::TryFrom;
use std::ffi::{CStr, CString};
use std::fmt::{self, Display};
use std::net;

use wl_expr::{Expr, ExprKind, Normal, Number, Symbol};
use wstp_sys::{WSErrorMessage, WSReady, WSReleaseErrorMessage, WSLINK};

//-----------------------------------
// Public re-exports and type aliases
//-----------------------------------

pub use wstp_sys as sys;

pub use crate::{
    error::Error,
    get::{Array, LinkStr},
    link_server::LinkServer,
};

// TODO: Remove this type alias after outside code has had time to update.
#[deprecated(note = "use wstp::Link")]
pub type WSTPLink = Link;

#[deprecated(note = "use wstp::Link")]
pub type WstpLink = Link;

// TODO: Make this function public from `wstp`?
pub(crate) use env::stdenv;


//======================================
// Source
//======================================

/// A WSTP link object.
///
/// [`WSClose()`][sys::WSClose] is called on the underlying [`WSLINK`] when
/// [`Drop::drop()`][Link::drop] is called for a value of this type.
///
/// *WSTP C API Documentation:* [`WSLINK`](https://reference.wolfram.com/language/ref/c/WSLINK.html)
///
/// *Wolfram Language Documentation:* [`LinkObject`](https://reference.wolfram.com/language/ref/LinkObject.html)
#[derive(Debug)]
#[repr(transparent)]
pub struct Link {
    raw_link: WSLINK,
}


// Use modified version of the code generated by `derive(RefCast)` which is marked `unsafe`.
//
// This is a workaround for https://github.com/dtolnay/ref-cast/issues/9.
impl Link {
    /// Transmute a `&mut WSLINK` into a `&mut Link`.
    ///
    /// For this operation to be safe, the caller must ensure:
    ///
    /// * the `WSLINK` is validly initialized.
    /// * they have unique ownership of the `WSLINK` value; no aliasing is possible.
    ///
    /// and the maintainer of this functionality must ensure:
    ///
    /// * The [`Link`] type is a `#[repr(transparent)]` wrapper around around a
    ///   single field of type [`WSLINK`][crate::sys::WSLINK].
    #[inline]
    unsafe fn unchecked_ref_cast_mut(from: &mut WSLINK) -> &mut Self {
        #[cfg(debug_assertions)]
        {
            #[allow(unused_imports)]
            use ::ref_cast::private::LayoutUnsized;
            ::ref_cast::private::assert_layout::<Self, WSLINK>(
                "Link",
                ::ref_cast::private::Layout::<Self>::SIZE,
                ::ref_cast::private::Layout::<WSLINK>::SIZE,
                ::ref_cast::private::Layout::<Self>::ALIGN,
                ::ref_cast::private::Layout::<WSLINK>::ALIGN,
            );
        }

        &mut *(from as *mut WSLINK as *mut Self)
    }
}

/// # Safety
///
/// [`Link`]s can be sent between threads, but they cannot be used from multiple
/// threads at once (unless `WSEnableLinkLock()` has been called on the link). So [`Link`]
/// satisfies [`Send`] but not [`Sync`].
///
/// **TODO:**
///   Add a wrapper type for [`Link`] which enforces that `WSEnableLinkLock()`
///   has been called, and implements [`Sync`].
unsafe impl Send for Link {}

/// Transport protocol used to communicate between two [`Link`] end points.
pub enum Protocol {
    /// Protocol type optimized for communication between two [`Link`] end points
    /// from within the same OS process.
    IntraProcess,
    /// Protocol type optimized for communication between two [`Link`] end points
    /// from the same machine — but not necessarily in the same OS process — using [shared
    /// memory](https://en.wikipedia.org/wiki/Shared_memory).
    SharedMemory,
    /// Protocol type for communication between two [`Link`] end points reachable
    /// across a network connection.
    TCPIP,
}

//======================================
// Impls
//======================================

/// # Creating WSTP link objects
impl Link {
    /// Create a new Loopback type link.
    ///
    /// *WSTP C API Documentation:* [`WSLoopbackOpen()`](https://reference.wolfram.com/language/ref/c/WSLoopbackOpen.html)
    pub fn new_loopback() -> Result<Self, Error> {
        unsafe {
            let mut err: std::os::raw::c_int = sys::MLEOK;
            let raw_link = sys::WSLoopbackOpen(stdenv()?.raw_env, &mut err);

            if raw_link.is_null() || err != sys::MLEOK {
                return Err(Error::from_code(err));
            }

            Ok(Link::unchecked_new(raw_link))
        }
    }

    /// Create a new named WSTP link using `protocol`.
    pub fn listen(protocol: Protocol, name: &str) -> Result<Self, Error> {
        let protocol_string = protocol.to_string();

        let strings: &[&str] = &[
            "-wstp",
            "-linkmode",
            "listen",
            "-linkprotocol",
            protocol_string.as_str(),
            "-linkname",
            name,
            // Prevent "Link created on: .." message from being printed.
            "-linkoptions",
            "MLDontInteract",
        ];

        Link::open_with_args(strings)
    }

    /// Connect to an existing named WSTP link.
    pub fn connect(protocol: Protocol, name: &str) -> Result<Self, Error> {
        Link::connect_with_options(protocol, name, &[])
    }

    /// Create a new WSTP [`TCPIP`][Protocol::TCPIP] link bound to `addr`.
    ///
    /// If `addr` yields multiple addresses, listening will be attempted with each of the
    /// addresses until one succeeds and returns the listener. If none of the addresses
    /// succeed in creating a listener, the error returned from the last attempt
    /// (the last address) is returned.
    pub fn tcpip_listen<A: net::ToSocketAddrs>(addr: A) -> Result<Self, Error> {
        let addrs = addr.to_socket_addrs().map_err(|err| {
            Error::custom(format!("error connecting to TCPIP Link address: {}", err))
        })?;

        // Try each address, returning the first one which binds for listening successfully.
        for_each_addr(addrs.collect(), |addr| {
            Link::listen(Protocol::TCPIP, &tcpip_link_name(&addr))
        })
    }

    /// Connect to an existing WSTP [`TCPIP`][Protocol::TCPIP] link listening at `addr`.
    ///
    /// If `addr` yields multiple addresses, a connection will be attempted with each of
    /// the addresses until a connection is successful. If none of the addresses result
    /// in a successful connection, the error returned from the last connection attempt
    /// (the last address) is returned.
    pub fn tcpip_connect<A: net::ToSocketAddrs>(addr: A) -> Result<Self, Error> {
        let addrs = addr.to_socket_addrs().map_err(|err| {
            Error::custom(format!("error connecting to TCPIP Link address: {}", err))
        })?;

        // Try each address, returning the first one which connects successfully.
        for_each_addr(addrs.collect(), |addr| {
            Link::connect(Protocol::TCPIP, &tcpip_link_name(&addr))
        })
    }

    /// Open a WSTP [`Protocol::TCPIP`] connection to a [`LinkServer`].
    ///
    /// If `addrs` yields multiple addresses, a connection will be attempted with each of
    /// the addresses until a connection is successful. If none of the addresses result
    /// in a successful connection, the error returned from the last connection attempt
    /// (the last address) is returned.
    pub fn connect_to_link_server<A: net::ToSocketAddrs>(
        addrs: A,
    ) -> Result<Self, Error> {
        let addrs = addrs.to_socket_addrs().map_err(|err| {
            Error::custom(format!("error connecting to LinkServer address: {}", err))
        })?;

        // Try each address, returning the first one which connects successfully.
        for_each_addr(addrs.collect(), |addr| {
            let mut link = Link::connect_with_options(
                Protocol::TCPIP,
                &tcpip_link_name(&addr),
                // Pass the magic option which signals that we're connecting to a
                // LinkServer, not just a normal Link.
                &["MLUseUUIDTCPIPConnection"],
            )?;

            // TODO: Should we activate here, or let the caller do this?
            let () = link.activate()?;

            return Ok(link);
        })
    }

    pub fn connect_with_options(
        protocol: Protocol,
        name: &str,
        options: &[&str],
    ) -> Result<Self, Error> {
        let protocol_string = protocol.to_string();

        let mut strings: Vec<&str> = vec![
            "-wstp",
            // "-linkconnect",
            "-linkmode",
            "connect",
            "-linkprotocol",
            protocol_string.as_str(),
            "-linkname",
            name,
        ];

        if !options.is_empty() {
            strings.push("-linkoptions");
            strings.extend(options);
        }

        Link::open_with_args(&strings)
    }

    /// *WSTP C API Documentation:* [`WSOpenArgcArgv()`](https://reference.wolfram.com/language/ref/c/WSOpenArgcArgv.html)
    ///
    /// This function can be used to create a [`Link`] of any protocol and mode. Prefer
    /// to use one of the constructor methods listed below when you know the type of link
    /// to be created.
    ///
    /// * [`Link::listen()`]
    /// * [`Link::connect()`]
    /// * [`Link::tcpip_listen()`]
    /// * [`Link::tcpip_connect()`]
    /// * [`Link::connect_to_link_server()`]
    // * [`Link::launch()`]
    // * [`Link::parent_connect()`]
    pub fn open_with_args(args: &[&str]) -> Result<Self, Error> {
        // NOTE: Before returning, we must convert these back into CString's to
        //       deallocate them.
        let mut c_strings: Vec<*mut i8> = args
            .into_iter()
            .map(|&str| {
                CString::new(str)
                    .expect("failed to create CString from WSTP link open argument")
                    .into_raw()
            })
            .collect();

        let mut err: std::os::raw::c_int = sys::MLEOK;

        let raw_link = unsafe {
            sys::WSOpenArgcArgv(
                stdenv()?.raw_env,
                i32::try_from(c_strings.len()).unwrap(),
                c_strings.as_mut_ptr(),
                &mut err,
            )
        };

        // Convert the `*mut i8` C strings back into owned CString's, so that they are
        // deallocated.
        for c_string in c_strings {
            unsafe {
                let _ = CString::from_raw(c_string);
            }
        }

        if raw_link.is_null() || err != sys::MLEOK {
            return Err(Error::from_code(err));
        }

        Ok(Link { raw_link })
    }

    pub unsafe fn unchecked_new(raw_link: WSLINK) -> Self {
        Link { raw_link }
    }

    /// *WSTP C API Documentation:* [`WSActivate()`](https://reference.wolfram.com/language/ref/c/WSActivate.html)
    pub fn activate(&mut self) -> Result<(), Error> {
        // Note: WSActivate() returns 0 in the event of an error, and sets an error
        //       code retrievable by WSError().
        if unsafe { sys::WSActivate(self.raw_link) } == 0 {
            return Err(self.error_or_unknown());
        }

        Ok(())
    }

    /// Close this end of the link.
    ///
    /// *WSTP C API Documentation:* [`WSClose()`](https://reference.wolfram.com/language/ref/c/WSClose.html)
    pub fn close(self) {
        // Note: The link is closed when `self` is dropped.
    }
}

/// # Link properties
impl Link {
    /// Get the name of this link.
    ///
    /// *WSTP C API Documentation:* [`WSLinkName()`](https://reference.wolfram.com/language/ref/c/WSLinkName.html)
    pub fn link_name(&self) -> String {
        let Link { raw_link } = *self;

        unsafe {
            let name: *const i8 = self::sys::WSName(raw_link as *mut _);
            CStr::from_ptr(name).to_str().unwrap().to_owned()
        }
    }

    /// Check if there is data ready to be read from this link.
    ///
    /// *WSTP C API Documentation:* [`WSReady()`](https://reference.wolfram.com/language/ref/c/WSReady.html)
    pub fn is_ready(&self) -> bool {
        let Link { raw_link } = *self;

        unsafe { WSReady(raw_link) != 0 }
    }

    /// *WSTP C API Documentation:* [`WSIsLinkLoopback()`](https://reference.wolfram.com/language/ref/c/WSIsLinkLoopback.html)
    pub fn is_loopback(&self) -> bool {
        let Link { raw_link } = *self;

        1 == unsafe { sys::WSIsLinkLoopback(raw_link) }
    }

    /// Returns an [`Error`] describing the last error to occur on this link.
    ///
    /// # Examples
    ///
    /// **TODO:** Example of getting an error code.
    pub fn error(&self) -> Option<Error> {
        let Link { raw_link } = *self;

        let (code, message): (i32, *const i8) =
            unsafe { (sys::WSError(raw_link), WSErrorMessage(raw_link)) };

        if code == sys::MLEOK || message.is_null() {
            return None;
        }

        let string: String = unsafe {
            let cstr = CStr::from_ptr(message);
            let string = cstr.to_str().unwrap().to_owned();

            WSReleaseErrorMessage(raw_link, message);
            // TODO: Should this method clear the error? If it does, it should at least be
            //       '&mut self'.
            // WSClearError(link);

            string
        };

        return Some(Error {
            code: Some(code),
            message: string,
        });
    }

    /// Returns a string describing the last error to occur on this link.
    ///
    /// TODO: If the most recent operation was successful, does the error message get
    ///       cleared?
    ///
    /// *WSTP C API Documentation:* [`WSErrorMessage()`](https://reference.wolfram.com/language/ref/c/WSErrorMessage.html)
    pub fn error_message(&self) -> Option<String> {
        self.error().map(|Error { message, code: _ }| message)
    }

    /// Helper to create an [`Error`] instance even if the underlying link does not have
    /// an error code set.
    pub(crate) fn error_or_unknown(&self) -> Error {
        self.error()
            .unwrap_or_else(|| Error::custom("unknown error occurred on WSLINK".into()))
    }

    /// Clear errors on this link.
    ///
    /// *WSTP C API Documentation:* [`WSClearError()`](https://reference.wolfram.com/language/ref/c/WSClearError.html)
    pub fn clear_error(&mut self) {
        let Link { raw_link } = *self;

        unsafe {
            sys::WSClearError(raw_link);
        }
    }

    /// *WSTP C API Documentation:* [`WSLINK`](https://reference.wolfram.com/language/ref/c/WSLINK.html)
    pub unsafe fn raw_link(&self) -> WSLINK {
        let Link { raw_link } = *self;
        raw_link
    }

    /// *WSTP C API Documentation:* [`WSUserData`](https://reference.wolfram.com/language/ref/c/WSUserData.html)
    pub unsafe fn user_data(&self) -> (*mut std::ffi::c_void, sys::WSUserFunction) {
        let Link { raw_link } = *self;

        let mut user_func: sys::WSUserFunction = None;

        let data_obj: *mut std::ffi::c_void = sys::WSUserData(raw_link, &mut user_func);

        (data_obj, user_func)
    }

    /// *WSTP C API Documentation:* [`WSSetUserData`](https://reference.wolfram.com/language/ref/c/WSSetUserData.html)
    pub unsafe fn set_user_data(
        &mut self,
        data_obj: *mut std::ffi::c_void,
        user_func: sys::WSUserFunction,
    ) {
        let Link { raw_link } = *self;

        sys::WSSetUserData(raw_link, data_obj, user_func);
    }
}

/// # Reading and writing expressions
impl Link {
    /// Flush out any buffers containing data waiting to be sent on this link.
    ///
    /// *WSTP C API Documentation:* [`WSFlush()`](https://reference.wolfram.com/language/ref/c/WSFlush.html)
    pub fn flush(&mut self) -> Result<(), Error> {
        if unsafe { sys::WSFlush(self.raw_link) } == 0 {
            return Err(self.error_or_unknown());
        }

        Ok(())
    }

    /// *WSTP C API Documentation:* [`WSGetNext()`](https://reference.wolfram.com/language/ref/c/WSGetNext.html)
    pub fn raw_get_next(&mut self) -> Result<i32, Error> {
        let type_ = unsafe { sys::WSGetNext(self.raw_link) };

        if type_ == sys::WSTKERR {
            return Err(self.error_or_unknown());
        }

        Ok(type_)
    }

    /// *WSTP C API Documentation:* [`WSNextPacket()`](https://reference.wolfram.com/language/ref/c/WSNextPacket.html)
    pub fn raw_next_packet(&mut self) -> Result<i32, Error> {
        let type_ = unsafe { sys::WSNextPacket(self.raw_link) };

        if type_ == sys::ILLEGALPKT {
            return Err(self.error_or_unknown());
        }

        Ok(type_)
    }

    /// *WSTP C API Documentation:* [`WSNewPacket()`](https://reference.wolfram.com/language/ref/c/WSNewPacket.html)
    pub fn new_packet(&mut self) -> Result<(), Error> {
        if unsafe { sys::WSNewPacket(self.raw_link) } == 0 {
            return Err(self.error_or_unknown());
        }

        Ok(())
    }

    /// Read an expression off of this link.
    pub fn get_expr(&mut self) -> Result<Expr, Error> {
        get_expr(self)
    }

    /// Write an expression to this link.
    pub fn put_expr(&mut self, expr: &Expr) -> Result<(), Error> {
        match expr.kind() {
            ExprKind::Normal(Normal { head, contents }) => {
                self.put_raw_type(i32::from(sys::WSTKFUNC))?;
                self.put_arg_count(contents.len())?;

                let _: () = self.put_expr(&*head)?;

                for elem in contents {
                    let _: () = self.put_expr(elem)?;
                }
            },
            ExprKind::Symbol(symbol) => {
                self.put_symbol(symbol.as_str())?;
            },
            ExprKind::String(string) => {
                self.put_str(string.as_str())?;
            },
            ExprKind::Number(Number::Integer(int)) => {
                self.put_i64(*int)?;
            },
            ExprKind::Number(Number::Real(real)) => {
                self.put_f64(**real)?;
            },
        }

        Ok(())
    }

    /// Transfer an expression from this link to another.
    ///
    /// # Example
    ///
    /// Transfer an expression between two loopback links:
    ///
    /// ```
    /// use wstp::Link;
    ///
    /// let mut a = Link::new_loopback().unwrap();
    /// let mut b = Link::new_loopback().unwrap();
    ///
    /// // Put an expression into `a`
    /// a.put_i64(5).unwrap();
    ///
    /// // Transfer it to `b`
    /// a.transfer_expr_to(&mut b).unwrap();
    ///
    /// assert_eq!(b.get_i64().unwrap(), 5);
    /// ```
    ///
    /// *WSTP C API Documentation:* [`WSTransferExpression()`](https://reference.wolfram.com/language/ref/c/WSTransferExpression.html)
    pub fn transfer_expr_to(&mut self, dest: &mut Link) -> Result<(), Error> {
        let result = unsafe { sys::WSTransferExpression(dest.raw_link, self.raw_link) };

        if result == 0 {
            return Err(self.error_or_unknown());
        }

        Ok(())
    }
}

//======================================
// Read from the link
//======================================

fn get_expr(link: &mut Link) -> Result<Expr, Error> {
    use wstp_sys::{WSTKFUNC, WSTKINT, WSTKREAL, WSTKSTR, WSTKSYM};

    let type_: i32 = link.get_raw_type()?;

    let expr: Expr = match type_ as u8 {
        WSTKINT => Expr::number(Number::Integer(link.get_i64()?)),
        WSTKREAL => {
            let real: wl_expr::F64 = match wl_expr::F64::new(link.get_f64()?) {
                Ok(real) => real,
                // TODO: Try passing a NaN value or a BigReal value through WSLINK.
                Err(_is_nan) => {
                    return Err(Error::custom(format!(
                        "NaN value passed on WSLINK cannot be used to construct an Expr"
                    )))
                },
            };
            Expr::number(Number::Real(real))
        },
        WSTKSTR => Expr::string(link.get_string_ref()?.to_str()),
        WSTKSYM => {
            let symbol_link_str = link.get_symbol_ref()?;
            let symbol_str = symbol_link_str.to_str();

            let symbol: Symbol = match Symbol::new(symbol_str) {
                Some(sym) => sym,
                None => {
                    return Err(Error::custom(format!(
                        "Symbol name `{}` has no context",
                        symbol_str
                    )))
                },
            };

            Expr::symbol(symbol)
        },
        WSTKFUNC => {
            let arg_count = link.get_arg_count()?;

            let head = link.get_expr()?;

            let mut contents = Vec::with_capacity(arg_count);
            for _ in 0..arg_count {
                contents.push(link.get_expr()?);
            }

            Expr::normal(head, contents)
        },
        _ => return Err(Error::custom(format!("unknown WSLINK type: {}", type_))),
    };

    Ok(expr)
}

//======================================
// Write to the link
//======================================

//======================================
// Utilities
//======================================

fn for_each_addr<T, F>(addrs: Vec<net::SocketAddr>, mut func: F) -> Result<T, Error>
where
    F: FnMut(net::SocketAddr) -> Result<T, Error>,
{
    let mut last_error = None;

    for addr in addrs {
        match func(addr) {
            Ok(result) => return Ok(result),
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error
        .unwrap_or_else(|| Error::custom(format!("socket address list is empty"))))
}

/// Construct an address string in the special syntax used by WSTP.
fn tcpip_link_name(addr: &net::SocketAddr) -> String {
    format!("{}@{}", addr.port(), addr.ip())
}

//======================================
// Formatting impls
//======================================

impl Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let str = match self {
            Protocol::IntraProcess => "IntraProcess",
            Protocol::SharedMemory => "SharedMemory",
            Protocol::TCPIP => "TCPIP",
        };

        write!(f, "{}", str)
    }
}

//======================================
// Drop impls
//======================================

impl Drop for Link {
    fn drop(&mut self) {
        let Link { raw_link } = *self;

        unsafe {
            sys::WSClose(raw_link);
        }
    }
}
