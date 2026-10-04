//! The shared region: header, then the ring. Page-aligned and committed up
//! front; the driver locks these pages while the attach request is pending.

use windows::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};

use crate::abi::{Header, BYTES_PER_FRAME, HEADER_BYTES, MAGIC, VERSION};
use crate::VaioError;

pub struct Region {
    ptr: *mut u8,
    len: usize,
    capacity: u32,
}

// SAFETY: the region is plain memory; all shared fields are atomics, and the
// ring's frames are only written by the driver (or a test's fake driver) and
// read by the one reader.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    pub fn new(capacity: u32, target: u32) -> Result<Region, VaioError> {
        let len = HEADER_BYTES + capacity as usize * BYTES_PER_FRAME;
        // SAFETY: plain allocation; checked for null below.
        let ptr = unsafe { VirtualAlloc(None, len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }.cast::<u8>();
        if ptr.is_null() {
            return Err(VaioError::Io("could not allocate the VAIO ring".into()));
        }
        // SAFETY: freshly committed, zeroed, page-aligned and at least a Header long.
        unsafe {
            let h = ptr.cast::<Header>();
            (*h).magic = MAGIC;
            (*h).version = VERSION;
            (*h).capacity_frames = capacity;
            (*h).target_frames = target;
        }
        Ok(Region { ptr, len, capacity })
    }

    pub fn header(&self) -> &Header {
        // SAFETY: valid for the region's lifetime; see `new`.
        unsafe { &*self.ptr.cast::<Header>() }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// For the attach request's output buffer.
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// The ring's frame `frame` (modulo capacity).
    ///
    /// # Safety
    /// The caller is the ring's only writer for that frame (the driver, or a
    /// test's fake driver), and no reference to it outlives the next write.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn frame_mut(&self, frame: u64) -> &mut [f32; 2] {
        let at = HEADER_BYTES + (frame % u64::from(self.capacity)) as usize * BYTES_PER_FRAME;
        &mut *self.ptr.add(at).cast::<[f32; 2]>()
    }

    pub(crate) fn frame(&self, frame: u64) -> [f32; 2] {
        let at = HEADER_BYTES + (frame % u64::from(self.capacity)) as usize * BYTES_PER_FRAME;
        // SAFETY: inside the region; a torn read of a frame being rewritten is
        // impossible because the driver never writes frames the reader may read
        // (it stays within `target` of the reader's position).
        unsafe { std::ptr::read_volatile(self.ptr.add(at).cast::<[f32; 2]>()) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: allocated in `new`; the attach thread (the only other owner)
        // holds an Arc, so this runs only after its request has completed.
        let _ = unsafe { VirtualFree(self.ptr.cast(), 0, MEM_RELEASE) };
    }
}
