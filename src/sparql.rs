//! Minimal bindings to LocalSearch's query library (libtinysparql-3.0): run a SPARQL query
//! against the running indexer, and get its change notifications.
//!
//! A TrackerSparqlConnection is thread-safe, so queries can run on worker threads.

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};

use gtk::glib;
use gtk::glib::translate::*;

#[link(name = "tinysparql-3.0")]
unsafe extern "C" {
    fn tracker_sparql_connection_bus_new(
        service: *const c_char,
        object_path: *const c_char,
        dbus: *mut c_void,
        error: *mut *mut glib::ffi::GError,
    ) -> *mut c_void;
    fn tracker_sparql_connection_query(
        conn: *mut c_void,
        sparql: *const c_char,
        cancellable: *mut c_void,
        error: *mut *mut glib::ffi::GError,
    ) -> *mut c_void;
    fn tracker_sparql_cursor_next(cursor: *mut c_void, cancellable: *mut c_void, error: *mut *mut glib::ffi::GError) -> c_int;
    fn tracker_sparql_cursor_get_n_columns(cursor: *mut c_void) -> c_int;
    fn tracker_sparql_cursor_get_string(cursor: *mut c_void, column: c_int, length: *mut c_long) -> *const c_char;
    fn tracker_sparql_connection_create_notifier(conn: *mut c_void) -> *mut c_void;
    fn tracker_notifier_event_get_event_type(event: *mut c_void) -> c_int;
    fn tracker_notifier_event_get_urn(event: *mut c_void) -> *const c_char;
    fn tracker_sparql_escape_string(literal: *const c_char) -> *mut c_char;
}

const SERVICE: &str = "org.freedesktop.LocalSearch3";

pub const EVENT_DELETE: i32 = 1;

/// A connection to the LocalSearch indexer.
pub struct Connection(*mut c_void);

// SAFETY: TrackerSparqlConnection is documented as thread-safe.
unsafe impl Send for Connection {}
unsafe impl Sync for Connection {}

impl Connection {
    pub fn new() -> Result<Self, glib::Error> {
        let service = CString::new(SERVICE).unwrap();
        let mut error = std::ptr::null_mut();
        // SAFETY: valid C string; null object path/D-Bus connection mean "defaults".
        let conn = unsafe { tracker_sparql_connection_bus_new(service.as_ptr(), std::ptr::null(), std::ptr::null_mut(), &mut error) };
        if conn.is_null() {
            // SAFETY: on failure the library set `error` (full ownership).
            return Err(unsafe { from_glib_full(error) });
        }
        Ok(Self(conn))
    }

    /// Run a query and collect every row (each column as a string; missing = "").
    pub fn query(&self, sparql: &str) -> Result<Vec<Vec<String>>, glib::Error> {
        let sparql = CString::new(sparql).map_err(|_| glib::Error::new(glib::FileError::Inval, "query contains a NUL byte"))?;
        let mut error = std::ptr::null_mut();
        // SAFETY: self.0 is a live connection; sparql is a valid C string.
        let cursor = unsafe { tracker_sparql_connection_query(self.0, sparql.as_ptr(), std::ptr::null_mut(), &mut error) };
        if cursor.is_null() {
            return Err(unsafe { from_glib_full(error) });
        }
        let mut rows = Vec::new();
        // SAFETY: cursor is valid until unref'd below; strings are copied before next().
        unsafe {
            let n = tracker_sparql_cursor_get_n_columns(cursor);
            let mut error = std::ptr::null_mut();
            while tracker_sparql_cursor_next(cursor, std::ptr::null_mut(), &mut error) != 0 {
                let row = (0..n)
                    .map(|c| {
                        let s = tracker_sparql_cursor_get_string(cursor, c, std::ptr::null_mut());
                        if s.is_null() { String::new() } else { CStr::from_ptr(s).to_string_lossy().into_owned() }
                    })
                    .collect();
                rows.push(row);
            }
            glib::gobject_ffi::g_object_unref(cursor as *mut _);
            if !error.is_null() {
                return Err(from_glib_full(error));
            }
        }
        Ok(rows)
    }

    /// A TrackerNotifier for this connection. Its "events" signal is delivered on the
    /// main context of the thread that calls this.
    pub fn notifier(&self) -> glib::Object {
        // SAFETY: returns a new reference to a GObject.
        unsafe { from_glib_full(tracker_sparql_connection_create_notifier(self.0) as *mut glib::gobject_ffi::GObject) }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { glib::gobject_ffi::g_object_unref(self.0 as *mut _) }
    }
}

/// Events from a notifier "events" signal argument (a GPtrArray of TrackerNotifierEvent):
/// (event type, urn).
pub fn notifier_events(value: &glib::Value) -> Vec<(i32, String)> {
    let mut out = Vec::new();
    // SAFETY: the "events" signal's 3rd argument is a boxed GPtrArray of TrackerNotifierEvent.
    unsafe {
        let array = glib::gobject_ffi::g_value_get_boxed(value.to_glib_none().0) as *mut glib::ffi::GPtrArray;
        if array.is_null() {
            return out;
        }
        for i in 0..(*array).len as usize {
            let event = *(*array).pdata.add(i);
            let urn = tracker_notifier_event_get_urn(event);
            if !urn.is_null() {
                out.push((tracker_notifier_event_get_event_type(event), CStr::from_ptr(urn).to_string_lossy().into_owned()));
            }
        }
    }
    out
}

/// Escape text for use inside a SPARQL string literal.
pub fn escape(s: &str) -> String {
    let Ok(c) = CString::new(s.replace('\0', "")) else { return String::new() };
    // SAFETY: valid C string in, newly allocated string out (freed here).
    unsafe {
        let out = tracker_sparql_escape_string(c.as_ptr());
        let r = CStr::from_ptr(out).to_string_lossy().into_owned();
        glib::ffi::g_free(out as *mut _);
        r
    }
}
