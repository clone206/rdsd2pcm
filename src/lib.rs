use std::ffi::c_uchar;
use std::os::raw::{c_char, c_float, c_int, c_void};
use std::ptr::NonNull;

#[repr(C)]
pub struct dsd2pcm_ctx_s {
    _private: [u8; 0],
}

#[link(name = "dsd2pcm")]
extern "C" {
    pub fn dsd2pcm_init() -> *mut dsd2pcm_ctx_s;
    pub fn dsd2pcm_destroy(ctx: *mut dsd2pcm_ctx_s);
    pub fn dsd2pcm_clone(ctx: *mut dsd2pcm_ctx_s) -> *mut dsd2pcm_ctx_s;
    pub fn dsd2pcm_reset(ctx: *mut dsd2pcm_ctx_s);
    pub fn dsd2pcm_translate(
        ctx: *mut dsd2pcm_ctx_s,
        samples: usize,
        src: *const c_uchar,
        src_stride: isize,
        lsbf: c_int,
        dst: *mut c_float,
        dst_stride: isize,
    );
}

pub struct Dsd2Pcm {
    ctx: NonNull<dsd2pcm_ctx_s>,
}

impl Dsd2Pcm {
    pub fn new() -> Option<Self> {
        unsafe {
            let ctx = dsd2pcm_init();
            NonNull::new(ctx).map(|ctx| Dsd2Pcm { ctx })
        }
    }

    pub fn reset(&mut self) {
        unsafe {
            dsd2pcm_reset(self.ctx.as_ptr());
        }
    }

    pub fn translate(
        &mut self,
        samples: usize,
        src: &[u8],
        src_stride: isize,
        lsbf: bool,
        dst: &mut [f32],
        dst_stride: isize,
    ) {
        unsafe {
            dsd2pcm_translate(
                self.ctx.as_ptr(),
                samples,
                src.as_ptr(),
                src_stride,
                lsbf as i32,
                dst.as_mut_ptr(),
                dst_stride,
            );
        }
    }
}

impl Clone for Dsd2Pcm {
    fn clone(&self) -> Self {
        unsafe {
            let ctx = dsd2pcm_clone(self.ctx.as_ptr());
            Dsd2Pcm {
                ctx: NonNull::new(ctx).expect("Failed to clone DSD2PCM context"),
            }
        }
    }
}

impl Drop for Dsd2Pcm {
    fn drop(&mut self) {
        unsafe {
            dsd2pcm_destroy(self.ctx.as_ptr());
        }
    }
}

unsafe impl Send for Dsd2Pcm {}
unsafe impl Sync for Dsd2Pcm {}
