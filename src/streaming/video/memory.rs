use crate::logging::log_line;
use anyhow::{Result, bail};
use std::ffi::c_void;
use std::sync::Mutex;
use vitasdk_sys::*;

const PHYCONT_GRANULARITY: u32 = 1024 * 1024;
const CDRAM_ALIGNMENT: u32 = 256 * 1024;
// Keep the name: it previously used USER_CDRAM_RW; the constant remains
// for the (now-unused) fallback path label. See `allocate`.

static IDLE_BUFFER: Mutex<Option<DecoderBlock>> = Mutex::new(None);

pub(super) struct DecoderBlock {
    uid: SceUID,
    pub(super) address: usize,
    capacity: u32,
    phycont: bool,
}

impl Drop for DecoderBlock {
    fn drop(&mut self) {
        unsafe { sceKernelFreeMemBlock(self.uid) };
    }
}

pub(super) struct DecoderFrameBuffer(Option<DecoderBlock>);

impl DecoderFrameBuffer {
    pub(super) fn acquire(size: u32) -> Result<Self> {
        let mut idle = IDLE_BUFFER.lock().unwrap();
        if idle.as_ref().is_some_and(|block| block.capacity >= size) {
            return Ok(Self(idle.take()));
        }
        drop(idle.take());
        drop(idle);

        match DecoderBlock::allocate(size, true) {
            Ok(block) => Ok(Self(Some(block))),
            Err(error) => {
                log_line!("PHYCONT-NC unavailable; trying CDRAM: {error:#}");
                DecoderBlock::allocate(size, false).map(|block| Self(Some(block)))
            }
        }
    }

    pub(super) fn is_phycont(&self) -> bool {
        self.0.as_ref().unwrap().phycont
    }

    pub(super) fn as_ptr(&self) -> *mut c_void {
        self.0.as_ref().unwrap().address as *mut c_void
    }
}

impl Drop for DecoderFrameBuffer {
    fn drop(&mut self) {
        let block = self.0.take().unwrap();
        let mut idle = IDLE_BUFFER.lock().unwrap();
        if idle.as_ref().is_none_or(|cached| {
            (block.phycont, block.capacity) > (cached.phycont, cached.capacity)
        }) {
            *idle = Some(block);
        }
    }
}

impl DecoderBlock {
    pub(super) fn allocate(size: u32, phycont: bool) -> Result<Self> {
        // `phycont == false` means the staging slots (DirectVideoOutput):
        // CDRAM (non-cached). The HW decoder DMA-writes into them, so they
        // must be NC — cached MAIN caused stale-cache macroblocking on
        // device (decoder DMA without cache invalidation).
        let (mem_type, alignment, pool) = if phycont {
            (SCE_KERNEL_MEMBLOCK_TYPE_USER_MAIN_PHYCONT_NC_RW, PHYCONT_GRANULARITY, "PHYCONT-NC")
        } else {
            (SCE_KERNEL_MEMBLOCK_TYPE_USER_CDRAM_RW, CDRAM_ALIGNMENT, "CDRAM")
        };
        let capacity = size.div_ceil(alignment) * alignment;
        let mut options = SceKernelAllocMemBlockOpt {
            size: size_of::<SceKernelAllocMemBlockOpt>() as u32,
            attr: SCE_KERNEL_ALLOC_MEMBLOCK_ATTR_HAS_ALIGNMENT,
            alignment,
            uidBaseBlock: 0,
            strBaseBlockName: std::ptr::null(),
            flags: 0,
            reserved: [0; 10],
        };
        // MAIN pools need the alignment options; PHYCONT-NC rejects them.
        let options = if phycont { std::ptr::null_mut() } else { &mut options };
        let uid = unsafe {
            sceKernelAllocMemBlock(c"xcloud_avcdec_frame".as_ptr(), mem_type, capacity, options)
        };
        if uid < 0 {
            bail!("{pool} decoder buffer allocation failed ({capacity} bytes): {uid:#x}; {}", free_memory_summary());
        }

        let mut block = Self { uid, address: 0, capacity, phycont };
        let mut base = std::ptr::null_mut();
        let ret = unsafe { sceKernelGetMemBlockBase(uid, &mut base) };
        if ret < 0 || base.is_null() {
            bail!("sceKernelGetMemBlockBase for {pool} decoder buffer failed: {ret:#x}");
        }
        block.address = base as usize;
        log_line!("Allocated {capacity} byte decoder frame buffer in {pool}");
        Ok(block)
    }
}

fn free_memory_summary() -> String {
    unsafe {
        let mut info = SceKernelFreeMemorySizeInfo {
            size: size_of::<SceKernelFreeMemorySizeInfo>() as i32,
            size_user: 0,
            size_cdram: 0,
            size_phycont: 0,
        };
        let ret = sceKernelGetFreeMemorySize(&mut info);
        if ret < 0 {
            return format!("free_memory=unavailable({ret:#x})");
        }
        format!(
            "free_memory=user:{} cdram:{} phycont:{}",
            info.size_user, info.size_cdram, info.size_phycont
        )
    }
}