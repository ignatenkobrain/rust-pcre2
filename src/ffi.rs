/*!
This module defines a low level and *mostly* safe abstraction around the
core PCRE2 regex primitives. Callers may still need to deal with some
unsafety, but this layer will take care of the obvious things, such as
resource management and error handling.
*/

use std::ptr;
use std::slice;

use libc::c_void;
use pcre2_sys::*;

use error::Error;

/// A low level representation of a compiled PCRE2 code object.
pub struct Code {
    code: *mut pcre2_code_8,
    // We hang on to this but don't use it so that it gets freed when the
    // compiled code gets freed. It's not clear whether this is necessary or
    // not, but presumably doesn't cost us much to be conservative.
    #[allow(dead_code)]
    ctx: CompileContext,
}

// SAFETY: Compiled PCRE2 code objects are immutable once built and explicitly
// safe to use from multiple threads simultaneously.
//
// One hitch here is that JIT compiling can write into a PCRE2 code object, but
// we only ever JIT compile immediately after first building the code object
// and before making it available to the caller.
unsafe impl Send for Code {}
unsafe impl Sync for Code {}

impl Drop for Code {
    fn drop(&mut self) {
        unsafe { pcre2_code_free_8(self.code) }
    }
}

impl Code {
    /// Compile the given pattern with the given options. If there was a
    /// problem compiling the pattern, then return an error.
    pub fn new(
        pattern: &str,
        options: u32,
        mut ctx: CompileContext,
    ) -> Result<Code, Error> {
        let (mut error_code, mut error_offset) = (0, 0);
        let code = unsafe {
            pcre2_compile_8(
                pattern.as_ptr(),
                pattern.len(),
                options,
                &mut error_code,
                &mut error_offset,
                ctx.as_mut_ptr(),
            )
        };
        if code.is_null() {
            Err(Error::compile(error_code, error_offset))
        } else {
            Ok(Code { code, ctx })
        }
    }

    /// JIT compile this code object.
    ///
    /// If there was a problem performing JIT compilation, then this returns
    /// an error.
    pub fn jit_compile(&mut self) -> Result<(), Error> {
        let error_code = unsafe {
            pcre2_jit_compile_8(self.code, PCRE2_JIT_COMPLETE)
        };
        if error_code == 0 {
            Ok(())
        } else {
            Err(Error::jit(error_code))
        }
    }

    /// Build and return an ordered sequence of all capture group names in this
    /// compiled regex.
    ///
    /// The returned vector has a slot for every capturing group (including the
    /// one corresponding to the entire regex, which is always unnamed). Groups
    /// that are unnamed are set to `None`.
    ///
    /// If there was a problem querying the compiled object for information,
    /// then this returns an error.
    pub fn capture_names(&self) -> Result<Vec<Option<String>>, Error> {
        // This is an object lesson in why C sucks. All we need is a map from
        // a name to a number, but we need to go through all sorts of
        // shenanigans to get it. In order to verify this code, see
        // https://www.pcre.org/current/doc/html/pcre2api.html
        // and search for PCRE2_INFO_NAMETABLE.

        let name_count = self.name_count()?;
        let size = self.name_entry_size()?;
        let table = unsafe {
            slice::from_raw_parts(self.raw_name_table()?, name_count * size)
        };

        let mut names = vec![None; self.capture_count()?];
        for i in 0..name_count {
            let entry = &table[i * size..(i + 1) * size];
            let name = &entry[2..];
            let nulat = name
                .iter()
                .position(|&b| b == 0)
                .expect("a NUL in name table entry");
            let index = (entry[0] as usize) << 8 | (entry[1] as usize);
            names[index] = String::from_utf8(name[..nulat].to_vec())
                .map(Some)
                // We require our pattern to be valid UTF-8, so all capture
                // names should also be valid UTF-8.
                .expect("valid UTF-8 for capture name");
        }
        Ok(names)
    }

    /// Return the underlying raw pointer to the code object.
    pub fn as_ptr(&self) -> *const pcre2_code_8 {
        self.code
    }

    /// Returns the raw name table, where each entry in the table corresponds
    /// to a mapping between a named capturing group and the index of that
    /// capturing group. The encoding for each item is as follows:
    ///
    /// * 2 bytes encoding the capture index (big-endian)
    /// * N bytes encoding the code units of the name
    /// * 1 byte for the NUL terminator
    /// * M padding bytes, corresponding to the difference in length between
    ///   this name and the longest name.
    ///
    /// In particular, each entry uses the same number of bytes.
    ///
    /// Entries are in alphabetical order.
    fn raw_name_table(&self) -> Result<*const u8, Error> {
        let mut bytes: *const u8 = ptr::null();
        let rc = unsafe {
            pcre2_pattern_info_8(
                self.as_ptr(),
                PCRE2_INFO_NAMETABLE,
                &mut bytes as *mut *const u8 as *mut c_void,
            )
        };
        if rc != 0 {
            Err(Error::info(rc))
        } else {
            Ok(bytes)
        }
    }

    /// Returns the number of named capturing groups.
    fn name_count(&self) -> Result<usize, Error> {
        let mut count: u32 = 0;
        let rc = unsafe {
            pcre2_pattern_info_8(
                self.as_ptr(),
                PCRE2_INFO_NAMECOUNT,
                &mut count as *mut u32 as *mut c_void,
            )
        };
        if rc != 0 {
            Err(Error::info(rc))
        } else {
            Ok(count as usize)
        }
    }

    /// Returns the entry size of each name in the name table.
    ///
    /// This appears to correspond to `3` plus the size of the longest named
    /// capturing group. The extra 3 bytes correspond to a NUL terminator and
    /// two prefix bytes corresponding to a big-endian encoding of the index
    /// of the capture group.
    fn name_entry_size(&self) -> Result<usize, Error> {
        let mut size: u32 = 0;
        let rc = unsafe {
            pcre2_pattern_info_8(
                self.as_ptr(),
                PCRE2_INFO_NAMEENTRYSIZE,
                &mut size as *mut u32 as *mut c_void,
            )
        };
        if rc != 0 {
            Err(Error::info(rc))
        } else {
            Ok(size as usize)
        }
    }

    /// Returns the total number of capturing groups in this regex. This
    /// includes the capturing group for the entire pattern, so that this is
    /// always 1 more than the number of syntactic groups in the pattern.
    pub fn capture_count(&self) -> Result<usize, Error> {
        let mut count: u32 = 0;
        let rc = unsafe {
            pcre2_pattern_info_8(
                self.as_ptr(),
                PCRE2_INFO_CAPTURECOUNT,
                &mut count as *mut u32 as *mut c_void,
            )
        };
        if rc != 0 {
            Err(Error::info(rc))
        } else {
            Ok(1 + count as usize)
        }
    }
}

/// A low level representation of PCRE2's compilation context.
pub struct CompileContext(*mut pcre2_compile_context_8);

// SAFETY: Compile contexts are safe to read from multiple threads
// simultaneously. No interior mutability is used, so Sync is safe.
unsafe impl Send for CompileContext {}
unsafe impl Sync for CompileContext {}

impl Drop for CompileContext {
    fn drop(&mut self) {
        unsafe { pcre2_compile_context_free_8(self.0) }
    }
}

impl CompileContext {
    /// Create a new empty compilation context.
    ///
    /// If memory could not be allocated for the context, then this panics.
    pub fn new() -> CompileContext {
        let ctx = unsafe {
            pcre2_compile_context_create_8(ptr::null_mut())
        };
        assert!(!ctx.is_null(), "could not allocate compile context");
        CompileContext(ctx)
    }

    /// Set the PCRE2 newline sequence.
    ///
    /// Valid values are: PCRE2_NEWLINE_CR, PCRE2_NEWLINE_LF,
    /// PCRE2_NEWLINE_CRLF, PCRE2_NEWLINE_ANYCRLF, PCRE2_NEWLINE_ANY or
    /// PCRE2_NEWLINE_NUL. Using any other value results in an error.
    pub fn set_newline(&mut self, value: u32) -> Result<(), Error> {
        let rc = unsafe { pcre2_set_newline_8(self.0, value) };
        if rc == 0 {
            Ok(())
        } else {
            Err(Error::option(rc))
        }
    }

    fn as_mut_ptr(&mut self) -> *mut pcre2_compile_context_8 {
        self.0
    }
}

/// A low level representation of a match data block.
///
/// Technically, a single match data block can be used with multiple regexes
/// (not simultaneously), but in practice, we just create a single match data
/// block for each regex for each thread it's used in.
pub struct MatchData {
    match_data: *mut pcre2_match_data_8,
    ovector_ptr: *const usize,
    ovector_count: u32,
}

// SAFETY: Match data blocks can be freely sent from one thread to another,
// but they do not support multiple threads using them simultaneously. We still
// implement Sync however, since we require mutable access to use the match
// data block for executing a search, which statically prevents simultaneous
// reading/writing. It is legal to read match data blocks from multiple threads
// simultaneously.
unsafe impl Send for MatchData {}
unsafe impl Sync for MatchData {}

impl Drop for MatchData {
    fn drop(&mut self) {
        unsafe { pcre2_match_data_free_8(self.match_data) }
    }
}

impl MatchData {
    /// Create a new match data block from a compiled PCRE2 code object.
    ///
    /// This panics if memory could not be allocated for the block.
    pub fn new(code: &Code) -> MatchData {
        let match_data = unsafe {
            pcre2_match_data_create_from_pattern_8(
                code.as_ptr(),
                ptr::null_mut(),
            )
        };
        assert!(!match_data.is_null(), "failed to allocate match data block");

        let ovector_ptr = unsafe { pcre2_get_ovector_pointer_8(match_data) };
        assert!(!ovector_ptr.is_null(), "got NULL ovector pointer");
        let ovector_count = unsafe { pcre2_get_ovector_count_8(match_data) };
        MatchData { match_data, ovector_ptr, ovector_count }
    }

    /// Execute PCRE2's primary match routine on the given subject string
    /// starting at the given offset. The provided options are passed to PCRE2
    /// as is.
    ///
    /// This returns false if no match occurred.
    ///
    /// Match offsets can be extracted via `ovector`.
    ///
    /// # Safety
    ///
    /// This routine is marked unsafe because it allows the caller to set
    /// arbitrary PCRE2 options. Some of those options can invoke undefined
    /// behavior when not used correctly. For example, if PCRE2_NO_UTF_CHECK
    /// is given and UTF mode is enabled and the given subject string is not
    /// valid UTF-8, then the result is undefined.
    pub unsafe fn find(
        &mut self,
        code: &Code,
        subject: &[u8],
        start: usize,
        options: u32,
    ) -> Result<bool, Error> {
        let rc = pcre2_match_8(
            code.as_ptr(),
            subject.as_ptr(),
            subject.len(),
            start,
            options,
            self.as_mut_ptr(),
            ptr::null_mut(),
        );
        if rc == PCRE2_ERROR_NOMATCH {
            Ok(false)
        } else if rc > 0 {
            Ok(true)
        } else {
            // We always create match data with
            // pcre2_match_data_create_from_pattern, so the ovector should
            // always be big enough.
            assert!(rc != 0, "ovector should never be too small");
            Err(Error::matching(rc))
        }
    }

    /// Return a mutable reference to the underlying match data.
    fn as_mut_ptr(&mut self) -> *mut pcre2_match_data_8 {
        self.match_data
    }

    /// Return the ovector corresponding to this match data.
    ///
    /// The ovector represents match offsets as pairs. This always returns
    /// N + 1 pairs (so 2*N + 1 offsets), where N is the number of capturing
    /// groups in the original regex.
    pub fn ovector(&self) -> &[usize] {
        // SAFETY: Both our ovector pointer and count are derived directly from
        // the creation of a valid match data block. One interesting question
        // here is whether the contents of the ovector are always initialized.
        // The PCRE2 documentation suggests that they are (so does testing),
        // but this isn't actually 100% clear!
        unsafe {
            slice::from_raw_parts(
                self.ovector_ptr,
                self.ovector_count as usize * 2,
            )
        }
    }
}
